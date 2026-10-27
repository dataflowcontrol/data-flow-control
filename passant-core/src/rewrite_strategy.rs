//! Shared rewrite-strategy interface and pipeline dispatch.
//!
//! Full-Push is the default when all policy aggregates are semiring-distributive.
//! Partial-Push is used only for non-semiring policy constraints.

use sqlparser::ast::{Query, SetExpr, Statement};
use std::sync::OnceLock;
use std::time::Instant;

use crate::full_push::FullPushEngine;
use crate::optimizer::RewriteStrategy;
use crate::parser::parse_query_with_dialect;
use crate::partial_push::PartialPushEngine;
use crate::policy_store::PolicyStore;
use crate::query_analysis::StatementAnalysis;
use crate::rewriter::{PassantRewriter, RewriteError, RewriteOptions};
use crate::semiring::SemiringAnalysis;

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
        Self::analyze_with_stats(
            original_sql,
            statement,
            options,
            store,
            &crate::aggregate_registry::AggregateRegistry::for_dialect(
                crate::sql::SqlDialect::DuckDb,
            ),
            None,
        )
    }

    pub(crate) fn analyze_with_stats(
        original_sql: &'a str,
        statement: &'a Statement,
        options: RewriteOptions,
        store: &PolicyStore,
        registry: &crate::aggregate_registry::AggregateRegistry,
        stats: Option<&crate::rewrite_stats::RewriteStatsCell>,
    ) -> Self {
        let analysis_start = Instant::now();
        let analysis = StatementAnalysis::from_statement_with_registry(statement, registry, stats);
        let (kind, select) = classify_statement(statement, registry);
        if let Some(stats) = stats {
            stats.add_elapsed_analysis(analysis_start.elapsed());
            stats.set_query_nodes(analysis.select_scopes.len());
        }

        let lookup_start = Instant::now();
        let tables = candidate_table_keys(&analysis);
        let sink_key = analysis.sink.clone();
        let candidate_lookup = store.candidate_scope_lookup(
            &tables,
            sink_key.as_ref(),
            crate::policy_store::MultiSourceLookupMode::Subset,
        );
        let mut candidate_count = 0usize;
        let semiring = store
            .semiring_for_candidate_iter(candidate_lookup.iter().inspect(|_| candidate_count += 1));
        if let Some(stats) = stats {
            stats.add_elapsed_candidate_lookup(lookup_start.elapsed());
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

/// Ordered collection of rewrite engines (built once per process).
pub struct RewritePipeline {
    engines: Vec<Box<dyn RewriteEngine>>,
}

static DEFAULT_PIPELINE: OnceLock<RewritePipeline> = OnceLock::new();

fn default_pipeline() -> &'static RewritePipeline {
    DEFAULT_PIPELINE.get_or_init(|| {
        let mut engines: Vec<Box<dyn RewriteEngine>> =
            vec![Box::new(FullPushEngine), Box::new(PartialPushEngine)];
        engines.sort_by_key(|engine| engine.priority());
        RewritePipeline { engines }
    })
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
        let store = rewriter.policy_store();
        if store.is_empty() {
            return Ok(sql.to_string());
        }

        let collect_stats = options.collect_stats;
        if collect_stats {
            rewriter.stats.reset(store.active_count());
        }
        rewriter.statement_summary.reset();

        let parse_start = Instant::now();
        let parse_dialect = options.effective_parse_dialect(rewriter.parse_dialect);
        let statement = parse_query_with_dialect(sql, parse_dialect)?;
        if collect_stats {
            rewriter.stats.add_elapsed_parse(parse_start.elapsed());
        }

        let scoped = crate::rewriter::output_scope::scoped_rewriter(rewriter, &statement)?;
        if let Some(scoped) = scoped {
            // Reuse the already-parsed statement; only active policies differ.
            scoped.stats.replace(rewriter.stats.snapshot());
            let result = self.rewrite_bound(&scoped, sql, options, &statement);
            rewriter.stats.replace(scoped.stats.snapshot());
            for scope in scoped.statement_summary.snapshot().scope_diagnostics {
                rewriter.statement_summary.record_scope(scope);
            }
            rewriter.ui_followup.set(scoped.ui_followup.take());
            return result;
        }
        self.rewrite_bound(rewriter, sql, options, &statement)
    }

    fn rewrite_bound(
        &self,
        rewriter: &PassantRewriter,
        sql: &str,
        options: RewriteOptions,
        statement: &Statement,
    ) -> Result<String, RewriteError> {
        let store = rewriter.policy_store();
        let collect_stats = options.collect_stats;
        if store.is_empty() {
            return Ok(sql.to_string());
        }
        let stats = collect_stats.then_some(&rewriter.stats);
        let request = RewriteRequest::analyze_with_stats(
            sql,
            statement,
            options.clone(),
            rewriter.policy_store(),
            &rewriter.aggregate_registry,
            stats,
        );
        ensure_ui_statement_supported(store, &request)?;

        if request.candidate_count == 0 {
            if rewriter.has_registered_policies() && request.kind == StatementKind::Passthrough {
                return Err(RewriteError::unsupported_statement(format!(
                    "unsupported statement form with registered policies: {}",
                    crate::parser::statement_label(request.statement)
                )));
            }
            if !store.has_enforcement_resolution_policies() {
                return Ok(sql.to_string());
            }
        }

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

    /// Shared process-wide pipeline (FullPush then PartialPush).
    pub fn shared() -> &'static RewritePipeline {
        default_pipeline()
    }
}

fn candidate_table_keys(
    analysis: &StatementAnalysis,
) -> std::collections::HashSet<crate::identifiers::TableKey> {
    let mut tables = analysis.table_keys.clone();
    for scope in &analysis.select_scopes {
        tables.extend(scope.scope.direct_base_tables.iter().cloned());
    }
    tables
}

fn ensure_ui_statement_supported(
    store: &PolicyStore,
    request: &RewriteRequest<'_>,
) -> Result<(), RewriteError> {
    if !store.has_ui_policies() {
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

fn classify_statement(
    statement: &Statement,
    registry: &crate::aggregate_registry::AggregateRegistry,
) -> (StatementKind, Option<SelectShape>) {
    match statement {
        Statement::Query(query) => (StatementKind::SelectQuery, select_shape(query, registry)),
        Statement::Insert { .. } => (StatementKind::Insert, None),
        Statement::Update { .. } => (StatementKind::Update, None),
        Statement::Merge { .. } => (StatementKind::Merge, None),
        Statement::Delete { .. } => (StatementKind::Delete, None),
        _ => (StatementKind::Passthrough, None),
    }
}

fn select_shape(
    query: &Query,
    registry: &crate::aggregate_registry::AggregateRegistry,
) -> Option<SelectShape> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    Some(SelectShape {
        is_aggregation: crate::rewriter::select_is_aggregation(select, registry),
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

    #[test]
    fn full_push_runs_before_partial_push() {
        let full = FullPushEngine;
        let partial = PartialPushEngine;
        assert!(full.priority() < partial.priority());
    }

    #[test]
    fn distributive_aggregation_uses_full_push() {
        let mut rewriter = PassantRewriter::new();
        rewriter
            .catalog_mut()
            .register_table("foo", vec!["category".into(), "amount".into()]);
        rewriter
            .register_policy(distributive_policy())
            .expect("fixture catalog validates policy");
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
    fn introspected_arbitrary_in_catalog_still_rewrites_kill_insert() {
        use crate::aggregate_registry::AggregateFunctionSnapshot;
        use crate::catalog::{CatalogSnapshot, CatalogTableInfo};
        use std::collections::HashMap;

        let policy_text = "SOURCE Receipts SINK Expenses CONSTRAINT ARBITRARY(Receipts.cat) != 'Meal' OR Expenses.biz_use <= 50.0 ON FAIL KILL";
        let sql = "INSERT INTO Expenses SELECT 1, 100.0 FROM Receipts";

        let mut rewriter = PassantRewriter::new();
        assert!(
            rewriter
                .register_validated_policy(crate::parse_policy_text(policy_text).unwrap())
                .is_err()
        );
        assert!(!rewriter.has_registered_policies());

        let mut tables = HashMap::new();
        tables.insert(
            "Receipts".to_string(),
            CatalogTableInfo {
                columns: vec!["cat".into(), "biz_use".into()],
                ..CatalogTableInfo::default()
            },
        );
        tables.insert(
            "Expenses".to_string(),
            CatalogTableInfo {
                columns: vec!["biz_use".into()],
                ..CatalogTableInfo::default()
            },
        );
        let mut rewriter = PassantRewriter::new();
        rewriter.apply_catalog_snapshot(CatalogSnapshot {
            dialect: Some("duckdb".to_string()),
            tables,
            aggregate_functions: vec![AggregateFunctionSnapshot {
                name: "arbitrary".to_string(),
                schema: None,
                aliases: vec![],
                classification: None,
                source: Some("introspected".to_string()),
            }],
            ..CatalogSnapshot::default()
        });
        rewriter
            .register_validated_policy(crate::parse_policy_text(policy_text).expect("parse"))
            .expect("register validated policy");
        assert!(
            rewriter
                .policy_store()
                .has_enforcement_resolution_policies(),
            "KILL policy should register as enforcement resolution"
        );
        assert_eq!(rewriter.policy_store().active_count(), 1);
        let with_catalog = rewriter.rewrite(sql);
        if let Err(err) = &with_catalog {
            panic!("rewrite with catalog failed: {err}");
        }
        let with_catalog = with_catalog.expect("rewrite ok");
        let summary = rewriter.last_statement_rewrite_summary();
        assert!(
            with_catalog.contains("passant_kill"),
            "expected kill rewrite with introspected arbitrary aggregate, got: {with_catalog}; summary={summary:?}"
        );
    }
}
