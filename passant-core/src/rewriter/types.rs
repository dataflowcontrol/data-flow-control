use std::collections::{HashMap, HashSet};

use sqlparser::ast::Expr;

use crate::aggregate_registry::AggregateRegistry;
use crate::catalog::TableCatalog;
use crate::identifiers::TableKey;
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
#[derive(Debug)]
pub struct PassantRewriter {
    pub(crate) store: PolicyStore,
    pub(crate) catalog: TableCatalog,
    pub(crate) aggregate_registry: AggregateRegistry,
    pub(crate) resolution_registry: crate::resolution_registry::ResolutionRegistry,
    pub(crate) parse_dialect: crate::sql::SqlDialect,
    pub(crate) stats: RewriteStatsCell,
    pub(crate) statement_summary: StatementRewriteSummaryCell,
    pub(crate) ui_followup: UiFollowupCell,
}

impl Default for PassantRewriter {
    fn default() -> Self {
        Self {
            store: PolicyStore::default(),
            catalog: TableCatalog::default(),
            aggregate_registry: AggregateRegistry::for_dialect(SqlDialect::default()),
            resolution_registry: crate::resolution_registry::ResolutionRegistry::default(),
            parse_dialect: SqlDialect::default(),
            stats: RewriteStatsCell::default(),
            statement_summary: StatementRewriteSummaryCell::default(),
            ui_followup: UiFollowupCell::default(),
        }
    }
}

impl Clone for PassantRewriter {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            catalog: self.catalog.clone(),
            aggregate_registry: self.aggregate_registry.clone(),
            resolution_registry: self.resolution_registry.clone(),
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
        let dialect = self.parse_dialect;
        let introspected = snapshot.aggregate_functions.clone();
        self.catalog.load_snapshot(snapshot);
        self.aggregate_registry =
            AggregateRegistry::for_dialect(dialect).merge_introspected(&introspected);
    }

    pub fn aggregate_registry(&self) -> &AggregateRegistry {
        &self.aggregate_registry
    }

    pub fn register_aggregate_function_name(
        &mut self,
        name: impl Into<String>,
        schema: Option<String>,
        classification: crate::aggregate_registry::AggregateClassification,
    ) {
        self.aggregate_registry
            .register_user_aggregate(name, schema, classification);
    }

    pub fn resolution_registry(&self) -> &crate::resolution_registry::ResolutionRegistry {
        &self.resolution_registry
    }

    pub fn register_resolution_splittability(&mut self, name: &str, splittable: bool) -> usize {
        self.resolution_registry.register(name, splittable)
    }

    pub fn resolution_properties(
        &self,
        resolution: &crate::policy::Resolution,
    ) -> crate::resolution_registry::ResolutionProperties {
        self.resolution_registry.properties(resolution)
    }

    pub fn sql_dialect(&self) -> crate::sql::SqlDialect {
        self.parse_dialect
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InsertOutputPolicyPlan {
    pub policy_indices: HashSet<usize>,
    pub aggregation_cte: String,
    pub policy_source_tables: HashSet<TableKey>,
    pub aggregate_aliases: HashMap<String, String>,
    pub aggregate_projections: Vec<(Expr, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InsertRewriteSite {
    Cte(String),
    Body,
}

#[derive(Debug, Clone)]
pub(crate) struct RewriteContext {
    pub(crate) require_output_labels: bool,
    pub(crate) sink_output_columns: Vec<sqlparser::ast::Ident>,
    pub(crate) joint_lineage: Option<super::joint_lineage::JointLineageContext>,
    pub(crate) joint_lineage_legacy:
        Option<std::rc::Rc<std::cell::RefCell<super::joint_lineage::TraversalState>>>,
    pub(crate) joint_resolution_prepared: bool,
    pub(crate) joint_resolution_endpoint: bool,
    pub(crate) joint_resolution_sources: HashSet<TableKey>,

    pub(crate) aggregate_registry: AggregateRegistry,
    pub(crate) resolution_registry: crate::resolution_registry::ResolutionRegistry,
    pub(crate) sink: Option<String>,
    pub(crate) sink_expr_by_column: HashMap<String, Expr>,
    pub(crate) ambiguous_output_columns: HashSet<String>,
    pub(crate) allow_partial_source_visibility: bool,
    /// When true, policy actions are deferred for the current SELECT and applied
    /// by an outer limit-first wrapper query.
    pub(crate) defer_policy_for_outer_limit: bool,
    /// Policy indices deferred to a parent aggregate scope (derived propagation).
    pub(crate) deferred_policy_indices: HashSet<usize>,
    /// INSERT policies deferred from inner scans to aggregation CTE + body SELECT.
    pub(crate) insert_output_policy: Option<InsertOutputPolicyPlan>,
    /// Current INSERT rewrite site when `insert_output_policy` is active.
    pub(crate) insert_rewrite_site: Option<InsertRewriteSite>,
    /// IN semijoin policy metrics collected during rewrite for limit-first wrappers.
    pub(crate) pending_in_semijoin_filters: Vec<crate::partial_push::ExtraDfcFilter>,
    pub(crate) collect_stats: bool,
    pub(crate) ui_mode: UiResolutionMode,
    pub(crate) ui_stream_endpoint: Option<String>,
}

impl Default for RewriteContext {
    fn default() -> Self {
        Self {
            require_output_labels: false,
            sink_output_columns: Vec::new(),
            joint_lineage: None,
            joint_lineage_legacy: None,
            joint_resolution_prepared: false,
            joint_resolution_endpoint: false,
            joint_resolution_sources: HashSet::new(),
            aggregate_registry: AggregateRegistry::for_dialect(SqlDialect::default()),
            resolution_registry: crate::resolution_registry::ResolutionRegistry::default(),
            sink: None,
            sink_expr_by_column: HashMap::new(),
            ambiguous_output_columns: HashSet::new(),
            allow_partial_source_visibility: false,
            defer_policy_for_outer_limit: false,
            deferred_policy_indices: HashSet::new(),
            insert_output_policy: None,
            insert_rewrite_site: None,
            pending_in_semijoin_filters: Vec::new(),
            collect_stats: false,
            ui_mode: UiResolutionMode::default(),
            ui_stream_endpoint: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyApplicability {
    Normal,
    RequiredSourceMissing,
}

#[cfg(test)]
mod resolution_metadata_tests {
    use super::*;
    use crate::{PolicyStore, Resolution};
    #[test]
    fn clone_and_branch_view_preserve_definition_order_and_splittability() {
        let mut rewriter = PassantRewriter::default();
        assert_eq!(
            rewriter.register_resolution_splittability("second_policy_function", true),
            0
        );
        assert_eq!(
            rewriter.register_resolution_splittability("first_policy_function", false),
            1
        );
        let cloned = rewriter.clone();
        let branch = PassantRewriter::with_branch_view(
            &PolicyStore::default(),
            vec![],
            TableCatalog::default(),
            SqlDialect::default(),
            rewriter.resolution_registry(),
        );
        for view in [&cloned, &branch] {
            assert_eq!(
                view.resolution_registry()
                    .registration_order("first_policy_function"),
                Some(1)
            );
            assert_eq!(
                view.resolution_registry()
                    .registration_order("second_policy_function"),
                Some(0)
            );
            assert!(
                view.resolution_properties(&Resolution::RelationUdf(
                    "second_policy_function".into()
                ))
                .splittable
            );
            assert!(
                !view
                    .resolution_properties(&Resolution::Udf("first_policy_function".into()))
                    .splittable
            );
        }
    }
}
