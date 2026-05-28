use std::collections::HashMap;

use sqlparser::ast::Expr;

use crate::catalog::TableCatalog;
use crate::policy_store::PolicyStore;
use crate::rewrite_stats::RewriteStatsCell;
use crate::sql::SqlDialect;

use super::plan::StatementRewriteSummaryCell;

/// Rewrite options passed through the pipeline.
#[derive(Debug, Default, Clone)]
pub struct RewriteOptions {
    /// When true, use partial-push rewrites (separate policy evaluation boundary) where required.
    pub use_partial_push: bool,
    /// When true, collect rewrite counters retrievable via `PassantRewriter::last_rewrite_stats`.
    pub collect_stats: bool,
    /// Optional parse dialect override; catalog snapshot dialect is used when unset.
    pub parse_dialect: Option<SqlDialect>,
}

impl RewriteOptions {
    pub fn effective_parse_dialect(&self, catalog_dialect: SqlDialect) -> SqlDialect {
        self.parse_dialect.unwrap_or(catalog_dialect)
    }
}

/// Policy storage and catalog facts for rewrite.
#[derive(Debug, Default)]
pub struct PassantRewriter {
    pub(crate) store: PolicyStore,
    pub(crate) catalog: TableCatalog,
    pub(crate) parse_dialect: crate::sql::SqlDialect,
    pub(crate) stats: RewriteStatsCell,
    pub(crate) statement_summary: StatementRewriteSummaryCell,
}

impl Clone for PassantRewriter {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            catalog: self.catalog.clone(),
            parse_dialect: self.parse_dialect,
            stats: RewriteStatsCell::default(),
            statement_summary: StatementRewriteSummaryCell::default(),
        }
    }
}

impl PassantRewriter {
    pub fn apply_catalog_snapshot(&mut self, snapshot: crate::catalog::CatalogSnapshot) {
        self.parse_dialect = snapshot.sql_dialect();
        self.catalog.load_snapshot(snapshot);
    }

    pub fn sql_dialect(&self) -> crate::sql::SqlDialect {
        self.parse_dialect
    }
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
