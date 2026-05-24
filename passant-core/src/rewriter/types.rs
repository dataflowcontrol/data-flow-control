use std::collections::HashMap;

use sqlparser::ast::Expr;

use crate::catalog::TableCatalog;
use crate::policy::PolicyIr;

/// Rewrite options passed through the pipeline.
#[derive(Debug, Default, Clone)]
pub struct RewriteOptions {
    /// When true, use partial-push rewrites (separate policy evaluation boundary) where required.
    pub use_partial_push: bool,
}

/// Policy storage and catalog facts for rewrite.
#[derive(Debug, Default, Clone)]
pub struct PassantRewriter {
    pub(crate) policies: Vec<PolicyIr>,
    pub(crate) catalog: TableCatalog,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyApplicability {
    Normal,
    RequiredSourceMissing,
}
