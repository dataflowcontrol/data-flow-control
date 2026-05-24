use std::collections::HashSet;

use sqlparser::ast::{Select, Statement};

use crate::catalog::TableCatalog;
pub use crate::diagnostics::RewriteError;
use crate::partial_push::ExtraDfcFilter;
use crate::policy::{PolicyIr, Resolution, parse_policy_text};

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
mod policy_expr;
mod projection;
mod select;
mod write_path;

use helpers::policy_description;

pub(crate) use columns::{
    collect_compound_columns_by_name, replace_identifiers, unqualify_columns,
};
pub(crate) use expr::{kill_expr, parse_expr, projected_column_name, resolver_expr};
pub(crate) use policy_expr::{build_compat_dfc_filter_expr, policy_applicability};
pub(crate) use projection::{
    apply_policy_having, ensure_projection_aliases, extract_policy_comparison, group_by_join_specs,
    outer_limited_projection_items, select_is_aggregation,
};

impl PassantRewriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_catalog(catalog: TableCatalog) -> Self {
        Self {
            policies: Vec::new(),
            catalog,
        }
    }

    pub fn catalog(&self) -> &TableCatalog {
        &self.catalog
    }

    pub fn catalog_mut(&mut self) -> &mut TableCatalog {
        &mut self.catalog
    }

    pub fn register_policy(&mut self, policy: PolicyIr) {
        self.policies.push(policy);
    }

    pub fn register_validated_policy(&mut self, policy: PolicyIr) -> Result<(), RewriteError> {
        if self.catalog.is_loaded() {
            self.catalog.validate_policy(&policy)?;
        }
        self.policies.push(policy);
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
        let Some(index) = self.policies.iter().position(|policy| {
            if let Some(sources) = sources
                && policy.sources() != sources
            {
                return false;
            }
            if let Some(sink) = sink
                && policy.sink() != Some(sink)
            {
                return false;
            }
            if let Some(constraint) = constraint
                && policy.constraint() != constraint
            {
                return false;
            }
            if let Some(on_fail) = on_fail
                && policy.resolution() != on_fail
            {
                return false;
            }
            if let Some(description) = description
                && policy_description(policy) != Some(description)
            {
                return false;
            }
            true
        }) else {
            return false;
        };
        self.policies.remove(index);
        true
    }

    pub fn policies(&self) -> &[PolicyIr] {
        &self.policies
    }

    pub fn dfc_policies(&self) -> Vec<PolicyIr> {
        self.policies
            .iter()
            .filter(|policy| matches!(policy, PolicyIr::CompatDfc { .. }))
            .cloned()
            .collect()
    }

    pub fn aggregate_policies(&self) -> Vec<PolicyIr> {
        self.policies
            .iter()
            .filter(|policy| matches!(policy, PolicyIr::CompatAggregate(_)))
            .cloned()
            .collect()
    }

    pub fn pgn_policies(&self) -> Vec<PolicyIr> {
        self.policies
            .iter()
            .filter(|policy| matches!(policy, PolicyIr::NativePgn(_)))
            .cloned()
            .collect()
    }

    pub fn has_compat_policies(&self) -> bool {
        self.policies.iter().any(|policy| {
            matches!(
                policy,
                PolicyIr::CompatDfc { .. } | PolicyIr::CompatAggregate(_)
            )
        })
    }

    pub fn has_registered_policies(&self) -> bool {
        !self.policies.is_empty()
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

    pub(crate) fn rewrite_statement_full_push(
        &self,
        statement: &mut Statement,
    ) -> Result<(), RewriteError> {
        self.rewrite_statement(statement)
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
