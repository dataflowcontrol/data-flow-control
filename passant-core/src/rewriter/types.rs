use std::collections::{HashMap, HashSet};

use sqlparser::ast::Expr;

use crate::catalog::TableCatalog;
use crate::policy_store::PolicyStore;
use crate::rewrite_stats::RewriteStatsCell;
use crate::sql::SqlDialect;

use std::sync::Mutex;

use super::plan::StatementRewriteSummaryCell;

/// Optional second statement produced by UI edited UPDATE rewrites.
#[derive(Debug, Default)]
pub struct UiFollowupCell(pub(crate) Mutex<Option<String>>);

impl UiFollowupCell {
    pub fn set(&self, sql: Option<String>) {
        if let Ok(mut guard) = self.0.lock() {
            *guard = sql;
        }
    }

    pub fn take(&self) -> Option<String> {
        self.0.lock().ok().and_then(|mut guard| guard.take())
    }
}

/// How UI resolution applies to UPDATE statements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UiUpdateMode {
    /// Approve or reject failing rows; no corrected values written to the stream.
    #[default]
    ApprovalOnly,
    /// Write corrected assignment values to the stream and apply via `UPDATE ... FROM`.
    EditedRows,
}

/// Rewrite options passed through the pipeline.
#[derive(Debug, Default, Clone)]
pub struct RewriteOptions {
    /// When true, use partial-push rewrites (separate policy evaluation boundary) where required.
    pub use_partial_push: bool,
    /// When true, collect rewrite counters retrievable via `PassantRewriter::last_rewrite_stats`.
    pub collect_stats: bool,
    /// Optional parse dialect override; catalog snapshot dialect is used when unset.
    pub parse_dialect: Option<SqlDialect>,
    /// Stream file path for `address_violating_rows` UI resolution (last UDF argument when set).
    pub ui_stream_endpoint: Option<String>,
    /// UPDATE-specific UI semantics when policies use `ON FAIL UI`.
    pub ui_update_mode: UiUpdateMode,
}

/// Statement-shape context for UI resolution rewrites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum UiResolutionMode {
    #[default]
    Disabled,
    InsertSelect,
    SelectResult,
    UpdateApprovalOnly,
    UpdateEditedRows,
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
    pub(crate) ui_followup: UiFollowupCell,
}

impl Clone for PassantRewriter {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            catalog: self.catalog.clone(),
            parse_dialect: self.parse_dialect,
            stats: RewriteStatsCell::default(),
            statement_summary: StatementRewriteSummaryCell::default(),
            ui_followup: UiFollowupCell::default(),
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
    pub(crate) ambiguous_output_columns: HashSet<String>,
    pub(crate) allow_partial_source_visibility: bool,
    pub(crate) collect_stats: bool,
    pub(crate) ui_mode: UiResolutionMode,
    pub(crate) ui_stream_endpoint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyApplicability {
    Normal,
    RequiredSourceMissing,
}
