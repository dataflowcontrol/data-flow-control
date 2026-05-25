use std::collections::HashSet;

use sqlparser::ast::{Select, Statement};

use crate::catalog::TableCatalog;
pub use crate::diagnostics::RewriteError;
use crate::identifiers::TableKey;
use crate::partial_push::ExtraDfcFilter;
use crate::policy::{PolicyIr, Resolution, parse_policy_text};
use crate::policy_store::{PolicyStore, PolicyStoreMemoryUsage};
use crate::rewrite_stats::{RewriteStats, RewriteStatsCell};

mod scope;
mod types;

pub(crate) use scope::TableScope;
pub(crate) use types::RewriteContext;
pub use types::{FinalizeQuery, PassantRewriter, RewriteOptions};

mod finalize;

mod aggregates;
mod columns;
mod exists;
mod expr;
mod helpers;
mod plan;
mod policy_expr;
mod projection;
mod select;
mod write_path;

pub(crate) use plan::{
    PolicyResolutionAction, plan_policy_filter_actions, resolve_scope_policies,
    scope_has_enforcement_policies,
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
pub(crate) use expr::{kill_expr, parse_expr, projected_column_name};
pub(crate) use helpers::direct_source_occurrence_counts;

impl PassantRewriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_catalog(catalog: TableCatalog) -> Self {
        Self {
            store: PolicyStore::default(),
            catalog,
            stats: RewriteStatsCell::default(),
            statement_summary: StatementRewriteSummaryCell::default(),
        }
    }

    pub fn from_policies(policies: Vec<PolicyIr>) -> Self {
        let mut rewriter = Self::new();
        for policy in policies {
            rewriter.register_policy(policy);
        }
        rewriter
    }

    pub(crate) fn with_policies_and_catalog(
        policies: Vec<PolicyIr>,
        catalog: TableCatalog,
    ) -> Self {
        let mut rewriter = Self::with_catalog(catalog);
        for policy in policies {
            rewriter.register_policy(policy);
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

    pub fn register_policy(&mut self, policy: PolicyIr) {
        let index = self.store.register(policy);
        self.finalize_scan_ready_expr(index);
    }

    pub fn register_validated_policy(&mut self, policy: PolicyIr) -> Result<(), RewriteError> {
        if self.catalog.is_loaded() {
            self.catalog.validate_policy(&policy)?;
        }
        let index = self.store.register(policy);
        self.finalize_scan_ready_expr(index);
        Ok(())
    }

    pub fn register_policy_text(&mut self, text: &str) -> Result<(), RewriteError> {
        let mut policy = parse_policy_text(text)?;
        if let PolicyIr::NativePgn(ref mut pgn) = policy {
            pgn.source_text = Some(text.to_string());
        }
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
            if let Some(on_fail) = on_fail
                && policy.resolution() != on_fail
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

    fn finalize_scan_ready_expr(&mut self, index: usize) {
        aggregates::finalize_policy_scan_ready(&mut self.store, index);
    }

    pub fn policies(&self) -> Vec<PolicyIr> {
        self.store.policies_vec()
    }

    pub fn dfc_policies(&self) -> Vec<PolicyIr> {
        self.store
            .dfc_policy_indices()
            .into_iter()
            .filter_map(|index| self.store.policy(index).cloned())
            .collect()
    }

    pub fn aggregate_policies(&self) -> Vec<PolicyIr> {
        self.store
            .aggregate_policy_indices()
            .into_iter()
            .filter_map(|index| self.store.policy(index).cloned())
            .collect()
    }

    pub fn pgn_policies(&self) -> Vec<PolicyIr> {
        self.store
            .pgn_policy_indices()
            .into_iter()
            .filter_map(|index| self.store.policy(index).cloned())
            .collect()
    }

    pub fn has_compat_policies(&self) -> bool {
        self.store.iter_active().any(|(_, policy)| {
            matches!(
                policy,
                PolicyIr::CompatDfc { .. } | PolicyIr::CompatAggregate(_)
            )
        })
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
        use crate::full_push::FullPushEngine;
        use crate::partial_push::PartialPushEngine;
        use crate::rewrite_strategy::RewritePipeline;

        let pipeline =
            RewritePipeline::new(vec![Box::new(FullPushEngine), Box::new(PartialPushEngine)]);
        pipeline.rewrite(self, sql, options)
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
        collect_stats: bool,
    ) -> Result<(), RewriteError> {
        self.rewrite_statement(statement, collect_stats)
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
