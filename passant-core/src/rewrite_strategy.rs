//! Shared rewrite-strategy interface and pipeline dispatch.
//!
//! Full-Push is the default when all policy aggregates are semiring-distributive.
//! Partial-Push is used only for non-semiring policy constraints.

use sqlparser::ast::{Query, SetExpr, Statement};
use std::time::Instant;

use crate::optimizer::RewriteStrategy;
use crate::parser::parse_query_with_dialect;
use crate::policy::Resolution;
use crate::policy_store::PolicyStore;
use crate::query_analysis::StatementAnalysis;
use crate::rewriter::{PassantRewriter, RewriteError, RewriteOptions};
use crate::semiring::SemiringAnalysis;
use crate::statement_tables::{statement_sink_key, statement_table_keys};

/// Planner-visible strategy label (re-exported for engine implementations).
pub use crate::optimizer::RewriteStrategy as StrategyKind;

/// Outcome of a single engine attempting a rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewriteAttempt {
    /// This engine produced rewritten SQL.
    Applied(String),
    /// Preconditions were not met; the pipeline should try the next engine.
    Skipped,
}

/// Classifies the top-level statement shape for strategy matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    SelectQuery,
    Insert,
    Update,
    Merge,
    Delete,
    Passthrough,
}

/// SELECT-specific shape derived once and shared by all engines.
#[derive(Debug, Clone)]
pub struct SelectShape {
    pub is_aggregation: bool,
    pub has_limit: bool,
}

/// Immutable analysis of a parsed statement plus rewrite options.
#[derive(Debug, Clone)]
pub struct RewriteRequest<'a> {
    pub original_sql: &'a str,
    pub statement: &'a Statement,
    pub options: RewriteOptions,
    pub kind: StatementKind,
    pub select: Option<SelectShape>,
    pub candidate_count: usize,
    pub semiring: SemiringAnalysis,
    pub analysis: StatementAnalysis,
}

impl<'a> RewriteRequest<'a> {
    pub fn analyze(
        original_sql: &'a str,
        statement: &'a Statement,
        options: RewriteOptions,
        store: &PolicyStore,
    ) -> Self {
        Self::analyze_with_stats(original_sql, statement, options, store, None)
    }

    pub(crate) fn analyze_with_stats(
        original_sql: &'a str,
        statement: &'a Statement,
        options: RewriteOptions,
        store: &PolicyStore,
        stats: Option<&crate::rewrite_stats::RewriteStatsCell>,
    ) -> Self {
        let lookup_start = Instant::now();
        let tables = statement_table_keys(statement);
        let sink = statement_sink_key(statement);
        let candidate_lookup = store.candidate_scope_lookup(
            &tables,
            sink.as_ref(),
            crate::policy_store::MultiSourceLookupMode::Subset,
        );
        let mut candidate_count = 0usize;
        let semiring = store
            .semiring_for_candidate_iter(candidate_lookup.iter().inspect(|_| candidate_count += 1));
        if let Some(stats) = stats {
            stats.add_elapsed_candidate_lookup(lookup_start.elapsed());
        }

        let analysis_start = Instant::now();
        let analysis = StatementAnalysis::from_statement_with_stats(statement, stats);
        let (kind, select) = classify_statement(statement);
        if let Some(stats) = stats {
            stats.add_elapsed_analysis(analysis_start.elapsed());
            stats.set_query_nodes(analysis.select_scopes.len());
        }

        Self {
            original_sql,
            statement,
            options,
            kind,
            select,
            semiring,
            candidate_count,
            analysis,
        }
    }

    pub fn requires_partial_push(&self) -> bool {
        self.options.use_partial_push || !self.semiring.all_distributive
    }
}

/// A rewrite engine pushes policy enforcement into SQL using one strategy.
pub trait RewriteEngine: Send + Sync {
    /// Which planner strategy this engine implements.
    fn kind(&self) -> RewriteStrategy;

    /// Lower values run first. Full-push is the default when policy aggregates are semiring.
    fn priority(&self) -> u8;

    /// Whether this engine should attempt the query.
    fn matches(&self, rewriter: &PassantRewriter, request: &RewriteRequest<'_>) -> bool;

    /// Execute the rewrite.
    fn rewrite(
        &self,
        rewriter: &PassantRewriter,
        request: &RewriteRequest<'_>,
    ) -> Result<RewriteAttempt, RewriteError>;
}

/// Ordered collection of rewrite engines.
pub struct RewritePipeline {
    engines: Vec<Box<dyn RewriteEngine>>,
}

impl RewritePipeline {
    pub fn new(mut engines: Vec<Box<dyn RewriteEngine>>) -> Self {
        engines.sort_by_key(|engine| engine.priority());
        Self { engines }
    }

    pub fn rewrite(
        &self,
        rewriter: &PassantRewriter,
        sql: &str,
        options: RewriteOptions,
    ) -> Result<String, RewriteError> {
        let collect_stats = options.collect_stats;
        if collect_stats {
            rewriter.stats.reset(rewriter.policy_store().active_count());
        }
        rewriter.statement_summary.reset();

        let parse_start = Instant::now();
        let parse_dialect = options.effective_parse_dialect(rewriter.parse_dialect);
        let statement = parse_query_with_dialect(sql, parse_dialect)?;
        if collect_stats {
            rewriter.stats.add_elapsed_parse(parse_start.elapsed());
        }

        let stats = collect_stats.then_some(&rewriter.stats);
        let request = RewriteRequest::analyze_with_stats(
            sql,
            &statement,
            options.clone(),
            rewriter.policy_store(),
            stats,
        );
        ensure_ui_statement_supported(rewriter.policy_store(), &request)?;

        let rewrite_start = Instant::now();
        for engine in &self.engines {
            if !engine.matches(rewriter, &request) {
                continue;
            }
            match engine.rewrite(rewriter, &request)? {
                RewriteAttempt::Applied(rewritten) => {
                    if collect_stats {
                        rewriter.stats.add_elapsed_rewrite(rewrite_start.elapsed());
                    }
                    return Ok(rewritten);
                }
                RewriteAttempt::Skipped => continue,
            }
        }
        if collect_stats {
            rewriter.stats.add_elapsed_rewrite(rewrite_start.elapsed());
        }
        if rewriter.has_registered_policies() && request.kind == StatementKind::Passthrough {
            return Err(RewriteError::unsupported_statement(format!(
                "unsupported statement form with registered policies: {}",
                crate::parser::statement_label(request.statement)
            )));
        }
        Ok(sql.to_string())
    }
}

fn store_has_ui_policies(store: &PolicyStore) -> bool {
    store
        .policies_vec()
        .iter()
        .any(|policy| policy.resolution() == Resolution::Ui)
}

fn ensure_ui_statement_supported(
    store: &PolicyStore,
    request: &RewriteRequest<'_>,
) -> Result<(), RewriteError> {
    if !store_has_ui_policies(store) {
        return Ok(());
    }
    if request.options.use_partial_push {
        return Err(RewriteError::unsupported_statement(
            "UI resolution is not supported with partial-push rewrites yet",
        ));
    }
    match request.kind {
        StatementKind::Insert | StatementKind::SelectQuery | StatementKind::Update => Ok(()),
        StatementKind::Merge | StatementKind::Delete | StatementKind::Passthrough => Ok(()),
    }
}

fn classify_statement(statement: &Statement) -> (StatementKind, Option<SelectShape>) {
    match statement {
        Statement::Query(query) => (StatementKind::SelectQuery, select_shape(query)),
        Statement::Insert { .. } => (StatementKind::Insert, None),
        Statement::Update { .. } => (StatementKind::Update, None),
        Statement::Merge { .. } => (StatementKind::Merge, None),
        Statement::Delete { .. } => (StatementKind::Delete, None),
        _ => (StatementKind::Passthrough, None),
    }
}

fn select_shape(query: &Query) -> Option<SelectShape> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    Some(SelectShape {
        is_aggregation: crate::rewriter::select_is_aggregation(select),
        has_limit: query_has_limit(query),
    })
}

pub(crate) fn query_has_limit(query: &Query) -> bool {
    query.limit.is_some() || query.offset.is_some() || query.fetch.is_some()
}

#[cfg(test)]
mod tests {
    use crate::full_push::FullPushEngine;
    use crate::partial_push::PartialPushEngine;
    use crate::policy::{PolicyIr, Resolution};
    use crate::rewrite_strategy::{RewriteEngine, RewriteRequest, StatementKind};
    use crate::rewriter::{PassantRewriter, RewriteOptions};

    fn distributive_policy() -> PolicyIr {
        PolicyIr::Pgn {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "max(foo.amount) > 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }
    }

    fn non_distributive_policy() -> PolicyIr {
        PolicyIr::Pgn {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "string_agg(foo.name, ',') = 'x'".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }
    }

    #[test]
    fn full_push_runs_before_partial_push() {
        let full = FullPushEngine;
        let partial = PartialPushEngine;
        assert!(full.priority() < partial.priority());
    }

    #[test]
    fn distributive_aggregation_uses_full_push() {
        let mut rewriter = PassantRewriter::new();
        rewriter.register_policy(distributive_policy());
        let statement = sqlparser::parser::Parser::parse_sql(
            &sqlparser::dialect::GenericDialect {},
            "SELECT category, sum(amount) FROM foo GROUP BY category",
        )
        .expect("parse")
        .pop()
        .expect("statement");
        let request = RewriteRequest::analyze(
            "SELECT category, sum(amount) FROM foo GROUP BY category",
            &statement,
            RewriteOptions::default(),
            rewriter.policy_store(),
        );
        assert_eq!(request.kind, StatementKind::SelectQuery);
        assert!(request.semiring.all_distributive);
        assert!(FullPushEngine.matches(&rewriter, &request));
        assert!(!PartialPushEngine.matches(&rewriter, &request));
    }

    #[test]
    fn non_distributive_policy_uses_partial_push() {
        let mut rewriter = PassantRewriter::new();
        rewriter.register_policy(non_distributive_policy());
        let statement = sqlparser::parser::Parser::parse_sql(
            &sqlparser::dialect::GenericDialect {},
            "SELECT id FROM foo",
        )
        .expect("parse")
        .pop()
        .expect("statement");
        let request = RewriteRequest::analyze(
            "SELECT id FROM foo",
            &statement,
            RewriteOptions::default(),
            rewriter.policy_store(),
        );
        assert!(!request.semiring.all_distributive);
        assert!(!FullPushEngine.matches(&rewriter, &request));
        assert!(PartialPushEngine.matches(&rewriter, &request));
    }
}
