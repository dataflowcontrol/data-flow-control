use std::collections::HashMap;

use sqlparser::ast::Expr;

use crate::catalog::TableCatalog;
use crate::policy_store::PolicyStore;
use crate::rewrite_stats::RewriteStatsCell;

use super::plan::StatementRewriteSummaryCell;

/// Rewrite options passed through the pipeline.
#[derive(Debug, Default, Clone)]
pub struct RewriteOptions {
    /// When true, use partial-push rewrites (separate policy evaluation boundary) where required.
    pub use_partial_push: bool,
    /// When true, collect rewrite counters retrievable via `PassantRewriter::last_rewrite_stats`.
    pub collect_stats: bool,
}

/// Policy storage and catalog facts for rewrite.
#[derive(Debug, Default)]
pub struct PassantRewriter {
    pub(crate) store: PolicyStore,
    pub(crate) catalog: TableCatalog,
    pub(crate) stats: RewriteStatsCell,
    pub(crate) statement_summary: StatementRewriteSummaryCell,
}

impl Clone for PassantRewriter {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            catalog: self.catalog.clone(),
            stats: RewriteStatsCell::default(),
            statement_summary: StatementRewriteSummaryCell::default(),
        }
    }
}

/// Aggregate policy finalization query bundle.
#[derive(Debug, Clone)]
pub struct FinalizeQuery {
    pub policy_id: String,
    pub sql: String,
    pub invalidate_sql: Option<String>,
    pub description: Option<String>,
    pub constraint: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SourceAggregate {
    pub(crate) sql: String,
    pub(crate) function_name: String,
    pub(crate) expr: Expr,
    pub(crate) is_sink_aggregate: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RewriteContext {
    pub(crate) sink: Option<String>,
    pub(crate) sink_expr_by_column: HashMap<String, Expr>,
    pub(crate) allow_partial_source_visibility: bool,
    pub(crate) collect_stats: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyApplicability {
    Normal,
    RequiredSourceMissing,
}
