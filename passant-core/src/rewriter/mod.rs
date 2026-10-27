mod joint_lineage;
pub(crate) mod output_scope;
use std::collections::HashSet;

use sqlparser::ast::{Select, Statement};

use crate::aggregate_registry::AggregateRegistry;
use crate::catalog::TableCatalog;
pub use crate::diagnostics::RewriteError;
use crate::identifiers::TableKey;
use crate::partial_push::ExtraDfcFilter;
use crate::policy::{PolicyIr, Resolution, parse_policy_text};
use crate::policy_store::{
    BranchPolicyEntry, PolicyStore, PolicyStoreMemoryUsage, PolicyStoreView,
};
use crate::rewrite_stats::{RewriteStats, RewriteStatsCell};
use crate::sql::SqlDialect;

mod scope;
mod types;

pub(crate) use scope::TableScope;
pub(crate) use types::RewriteContext;
pub use types::{PassantRewriter, RewriteOptions, UiFollowupCell, UiUpdateMode};

mod aggregates;
pub(crate) use aggregates::decompose_composed_aggregates;
mod columns;
pub(crate) mod constraint_preprocess;
pub(crate) use constraint_preprocess::preprocess_policy_constraint;
mod derived_policy;
pub(crate) mod dimensions;
mod exists;
mod expr;
mod helpers;
mod insert_output_policy;
pub(crate) mod limit;
mod plan;
mod policy_expr;
mod projection;
pub(crate) mod resolution;
mod select;
mod write_path;

pub(crate) use plan::{
    PolicyResolutionAction, apply_policy_resolution_actions, plan_policy_filter_actions,
    resolve_scope_policies, scope_has_enforcement_policies,
};
pub use plan::{
    ScopePlanDiagnostics, SelectRewritePlan, StatementRewriteSummary,
    plan_statement_rewrite_summary,
};
pub(crate) use projection::{
    apply_policy_having, ensure_projection_aliases, extract_policy_comparison_for_policy,
    group_by_join_specs, outer_limited_projection_items, select_is_aggregation,
};

use helpers::policy_description;

use plan::StatementRewriteSummaryCell;

pub(crate) use columns::{
    collect_compound_columns_by_name, replace_identifiers, unqualify_columns,
};
pub(crate) use expr::{expr_contains_aggregate, parse_expr, projected_column_name};
pub(crate) use helpers::direct_source_occurrence_counts;

impl PassantRewriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_catalog(catalog: TableCatalog) -> Self {
        Self {
            store: PolicyStore::default(),
            catalog,
            aggregate_registry: AggregateRegistry::for_dialect(SqlDialect::default()),
            resolution_registry: crate::resolution_registry::ResolutionRegistry::default(),
            parse_dialect: SqlDialect::default(),
            stats: RewriteStatsCell::default(),
            statement_summary: StatementRewriteSummaryCell::default(),
            ui_followup: UiFollowupCell::default(),
        }
    }

    pub fn from_policies(policies: Vec<PolicyIr>) -> Result<Self, RewriteError> {
        Self::from_policies_with_catalog(policies, TableCatalog::default())
    }

    pub fn from_policies_with_catalog(
        policies: Vec<PolicyIr>,
        catalog: TableCatalog,
    ) -> Result<Self, RewriteError> {
        let mut rewriter = Self::with_catalog(catalog);
        rewriter.register_policies(policies)?;
        Ok(rewriter)
    }

    /// Branch-local rewriter: small `PolicyStoreView` sharing parent interners and catalog facts.
    pub(crate) fn with_branch_view(
        parent: &PolicyStore,
        entries: Vec<BranchPolicyEntry>,
        catalog: TableCatalog,
        parse_dialect: SqlDialect,
        resolution_registry: &crate::resolution_registry::ResolutionRegistry,
    ) -> Self {
        let store = PolicyStoreView::build(parent, entries).into_store();
        let mut rewriter = Self {
            store,
            catalog,
            aggregate_registry: AggregateRegistry::for_dialect(parse_dialect),
            resolution_registry: resolution_registry.clone(),
            parse_dialect,
            stats: RewriteStatsCell::default(),
            statement_summary: StatementRewriteSummaryCell::default(),
            ui_followup: UiFollowupCell::default(),
        };
        let registry = rewriter.aggregate_registry.clone();
        for index in 0..rewriter.store.len() {
            if rewriter.store.compiled(index).is_some() {
                rewriter.finalize_scan_ready_expr(index, &registry);
            }
        }
        rewriter
    }

    pub(crate) fn policy_at(&self, index: usize) -> Option<&PolicyIr> {
        self.store.policy(index)
    }

    pub fn catalog(&self) -> &TableCatalog {
        &self.catalog
    }

    pub fn catalog_mut(&mut self) -> &mut TableCatalog {
        &mut self.catalog
    }

    pub(crate) fn policy_store(&self) -> &PolicyStore {
        &self.store
    }

    /// Every declared physical SOURCE/SINK must have explicit catalog metadata.
    pub fn register_policy(&mut self, policy: PolicyIr) -> Result<(), RewriteError> {
        self.register_policies(vec![policy])
    }

    pub fn register_validated_policy(&mut self, policy: PolicyIr) -> Result<(), RewriteError> {
        self.register_policy(policy)
    }

    /// Validate the entire batch before allocating stable policy identities.
    pub fn register_policies(&mut self, policies: Vec<PolicyIr>) -> Result<(), RewriteError> {
        let mut validated = Vec::with_capacity(policies.len());
        for policy in policies {
            let parsed = crate::policy_compile::parse_policy_constraint_with_registry(
                policy.constraint(),
                &self.aggregate_registry,
            )?;
            self.catalog
                .validate_pgn_policy_parsed(&policy, &parsed, &self.aggregate_registry)?;
            validated.push((policy, parsed));
        }
        let registry = self.aggregate_registry.clone();
        for (policy, parsed) in validated {
            let index = self.store.register_with_parsed(policy, parsed);
            self.finalize_scan_ready_expr(index, &registry);
        }
        Ok(())
    }

    pub fn register_policy_text(&mut self, text: &str) -> Result<(), RewriteError> {
        let policy = parse_policy_text(text)?;
        self.register_validated_policy(policy)
    }

    pub fn delete_policy(
        &mut self,
        sources: Option<&[String]>,
        sink: Option<&str>,
        constraint: Option<&str>,
        on_fail: Option<Resolution>,
        description: Option<&str>,
    ) -> bool {
        let sink_key = sink.map(TableKey::new);
        let candidate_ids = self
            .store
            .candidate_ids_for_delete_lookup(sources, sink_key.as_ref());
        let Some(index) = candidate_ids.into_iter().find_map(|index| {
            let policy = self.store.policy(index)?;
            if let Some(sources) = sources
                && policy.sources() != sources
            {
                return None;
            }
            if let Some(sink) = sink
                && policy.sink() != Some(sink)
            {
                return None;
            }
            if let Some(constraint) = constraint
                && policy.constraint() != constraint
            {
                return None;
            }
            if let Some(expected) = &on_fail
                && policy.resolution() != *expected
            {
                return None;
            }
            if let Some(description) = description
                && policy_description(policy) != Some(description)
            {
                return None;
            }
            Some(index)
        }) else {
            return false;
        };
        self.store.deactivate(index)
    }

    fn finalize_scan_ready_expr(&mut self, index: usize, registry: &AggregateRegistry) {
        aggregates::finalize_policy_scan_ready(&mut self.store, index, registry);
    }

    pub fn policy_by_index(&self, index: usize) -> Option<&PolicyIr> {
        self.store.policy_by_original_index(index)
    }

    pub fn policies(&self) -> Vec<PolicyIr> {
        self.store.policies_vec()
    }

    pub fn has_policies(&self) -> bool {
        !self.store.policy_indices().is_empty()
    }

    pub fn has_registered_policies(&self) -> bool {
        !self.store.is_empty()
    }

    pub fn rewrite(&self, sql: &str) -> Result<String, RewriteError> {
        self.rewrite_with_options(sql, RewriteOptions::default())
    }

    pub fn rewrite_with_options(
        &self,
        sql: &str,
        options: RewriteOptions,
    ) -> Result<String, RewriteError> {
        use crate::rewrite_strategy::RewritePipeline;

        self.ui_followup.set(None);
        RewritePipeline::shared().rewrite(self, sql, options)
    }

    /// When the last rewrite was an edited UI UPDATE, returns the follow-up `UPDATE ... FROM` SQL.
    pub fn last_ui_followup_sql(&self) -> Option<String> {
        self.ui_followup.take()
    }

    pub(crate) fn has_ui_policies(&self) -> bool {
        self.store.has_ui_policies()
    }

    /// Counters from the most recent rewrite when `RewriteOptions::collect_stats` was enabled.
    pub fn last_rewrite_stats(&self) -> RewriteStats {
        self.stats.snapshot()
    }

    /// Estimated memory footprint of the registered policy store.
    pub fn store_memory_usage(&self) -> PolicyStoreMemoryUsage {
        self.store.memory_usage()
    }

    /// Per-scope planning diagnostics from the most recent rewrite.
    pub fn last_statement_rewrite_summary(&self) -> StatementRewriteSummary {
        self.statement_summary.snapshot()
    }

    /// Plan policy candidate counts for each SELECT scope without mutating SQL.
    pub fn plan_statement_summary(
        &self,
        statement: &sqlparser::ast::Statement,
    ) -> StatementRewriteSummary {
        plan_statement_rewrite_summary(&self.store, statement)
    }

    pub(crate) fn rewrite_statement_full_push(
        &self,
        statement: &mut Statement,
        options: &RewriteOptions,
    ) -> Result<(), RewriteError> {
        self.rewrite_statement(statement, options)
    }

    pub(crate) fn rewrite_exists_subqueries_as_joins(
        &self,
        select: &mut Select,
    ) -> Result<HashSet<usize>, RewriteError> {
        self.rewrite_exists_subqueries_as_joins_impl(select)
    }

    pub(crate) fn rewrite_in_subqueries_as_joins(
        &self,
        select: &mut Select,
    ) -> Result<(HashSet<usize>, Vec<ExtraDfcFilter>), RewriteError> {
        self.rewrite_in_subqueries_as_joins_impl(select)
    }
}

#[cfg(test)]
mod registration_catalog_tests {
    use super::*;
    fn policy(sql: &str) -> PolicyIr {
        parse_policy_text(sql).unwrap()
    }
    fn catalog() -> TableCatalog {
        let mut catalog = TableCatalog::default();
        catalog.register_table("s.a", vec!["x".into(), "Quoted Column".into()]);
        catalog.register_table("target", vec!["x".into()]);
        catalog
    }

    #[test]
    fn every_physical_resolution_requires_explicit_tables_and_columns() {
        for resolution in [
            "REMOVE",
            "KILL",
            "RETRY('x')",
            "UDF repair",
            "RELATION UDF repair",
            "UI",
        ] {
            let mut absent = PassantRewriter::default();
            let p = policy(&format!(
                "SOURCE s.a SINK target CONSTRAINT s.a.x > 0 ON FAIL {resolution}"
            ));
            assert!(absent.register_policy(p.clone()).is_err());
            assert_eq!(absent.store.len(), 0);
            let mut known = PassantRewriter::with_catalog(catalog());
            known.register_policy(p).unwrap();
            let invalid = policy(&format!(
                "SOURCE s.a SINK target CONSTRAINT target.missing > 0 ON FAIL {resolution}"
            ));
            assert!(known.register_policy(invalid).is_err());
            assert_eq!(known.store.len(), 1);
        }
    }

    #[test]
    fn failed_batch_leaves_ids_and_existing_policy_unchanged() {
        let mut rewriter = PassantRewriter::with_catalog(catalog());
        let valid = policy("SOURCE s.a SINK target CONSTRAINT s.a.x > 0 ON FAIL RETRY('exact')");
        rewriter.register_policy(valid.clone()).unwrap();
        let invalid = policy("SOURCE missing CONSTRAINT TRUE ON FAIL REMOVE");
        assert!(
            rewriter
                .register_policies(vec![valid.clone(), invalid])
                .is_err()
        );
        assert_eq!(rewriter.store.len(), 1);
        assert_eq!(rewriter.policy_by_index(0), Some(&valid));
        rewriter.register_policy(valid.clone()).unwrap();
        assert_eq!(rewriter.policy_by_index(1), Some(&valid));
    }

    #[test]
    fn explicit_unloaded_catalog_aliases_and_quoted_columns_are_validated() {
        let mut rewriter = PassantRewriter::with_catalog(catalog());
        rewriter.register_policy_text("SOURCE s.a AS a SINK target AS t CONSTRAINT a.\"Quoted Column\" > 0 AND t.x > 0 ON FAIL REMOVE").unwrap();
        let bad =
            policy("SOURCE s.a AS a SINK target AS t CONSTRAINT a.missing > 0 ON FAIL REMOVE");
        assert!(rewriter.register_policy(bad).is_err());
        let cloned = rewriter.clone();
        assert!(cloned.catalog.table_exists("s.a"));
        assert_eq!(cloned.policies().len(), 1);
    }
}
