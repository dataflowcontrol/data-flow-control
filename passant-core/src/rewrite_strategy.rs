//! Shared rewrite-strategy interface and pipeline dispatch.
//!
//! Full-Push is the default when all policy aggregates are semiring-distributive.
//! Partial-Push is used only for non-semiring policy constraints.

use sqlparser::ast::{Query, SetExpr, Statement};

use crate::optimizer::RewriteStrategy;
use crate::parser::parse_query;
use crate::policy::PolicyIr;
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
    pub semiring: SemiringAnalysis,
}

impl<'a> RewriteRequest<'a> {
    pub fn analyze(
        original_sql: &'a str,
        statement: &'a Statement,
        options: RewriteOptions,
        policies: &[PolicyIr],
    ) -> Self {
        let (kind, select) = classify_statement(statement);
        Self {
            original_sql,
            statement,
            options,
            kind,
            select,
            semiring: crate::semiring::analyze_policies(policies),
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
        let statement = parse_query(sql)?;
        let request = RewriteRequest::analyze(sql, &statement, options, rewriter.policies());
        for engine in &self.engines {
            if !engine.matches(rewriter, &request) {
                continue;
            }
            match engine.rewrite(rewriter, &request)? {
                RewriteAttempt::Applied(rewritten) => {
                    return Ok(finalize_output(sql, rewritten, rewriter.policies()));
                }
                RewriteAttempt::Skipped => continue,
            }
        }
        Ok(sql.to_string())
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

pub(crate) fn finalize_output(
    original_sql: &str,
    rewritten: String,
    policies: &[PolicyIr],
) -> String {
    crate::rewriter::postprocess_rewritten_sql(original_sql, rewritten, policies)
}

#[cfg(test)]
mod tests {
    use crate::full_push::FullPushEngine;
    use crate::partial_push::PartialPushEngine;
    use crate::policy::{PolicyIr, Resolution};
    use crate::rewrite_strategy::{RewriteEngine, RewriteRequest, StatementKind};
    use crate::rewriter::{PassantRewriter, RewriteOptions};

    fn distributive_policy() -> PolicyIr {
        PolicyIr::CompatDfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(foo.amount) > 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }
    }

    fn non_distributive_policy() -> PolicyIr {
        PolicyIr::CompatDfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "avg(foo.amount) > 1".to_string(),
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
            rewriter.policies(),
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
            rewriter.policies(),
        );
        assert!(!request.semiring.all_distributive);
        assert!(!FullPushEngine.matches(&rewriter, &request));
        assert!(PartialPushEngine.matches(&rewriter, &request));
    }
}
