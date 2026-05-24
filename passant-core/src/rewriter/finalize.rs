use std::collections::HashSet;

use crate::identifiers::{PolicyId, TableKey};
use crate::policy::PolicyIr;

use super::RewriteError;
use super::aggregates::{
    aggregate_finalize_sql, aggregate_finalize_sql_fallback, aggregate_temp_column,
    policy_aggregate_temp_entries, rewrite_source_aggregates_for_finalize,
};
use super::scope::TableScope;
use super::types::{FinalizeQuery, PassantRewriter, SourceAggregate};

impl PassantRewriter {
    pub fn finalize_aggregate_policies(&self, sink_table: &str) -> Vec<(String, Option<String>)> {
        self.policies
            .iter()
            .filter_map(|policy| match policy {
                PolicyIr::CompatAggregate(policy)
                    if policy
                        .sink
                        .as_deref()
                        .is_none_or(|sink| sink.eq_ignore_ascii_case(sink_table)) =>
                {
                    Some((
                        PolicyId::from_aggregate_constraint(&policy.constraint).to_string(),
                        None,
                    ))
                }
                _ => None,
            })
            .collect()
    }

    pub fn finalize_aggregate_queries(&self, sink_table: &str) -> Vec<FinalizeQuery> {
        let aggregate_temp_columns = self
            .source_aggregate_temp_columns(sink_table, None)
            .unwrap_or_default();
        self.policies
            .iter()
            .filter_map(|policy| match policy {
                PolicyIr::CompatAggregate(policy)
                    if policy
                        .sink
                        .as_deref()
                        .is_none_or(|sink| sink.eq_ignore_ascii_case(sink_table)) =>
                {
                    let constraint = rewrite_source_aggregates_for_finalize(
                        &policy.constraint,
                        &policy.sources,
                        &aggregate_temp_columns,
                    )
                    .unwrap_or_else(|_| policy.constraint.clone());
                    let (sql, invalidate_sql) =
                        aggregate_finalize_sql(sink_table, &constraint, &policy.dimensions)
                            .unwrap_or_else(|_| {
                                aggregate_finalize_sql_fallback(sink_table, &constraint)
                            });
                    Some(FinalizeQuery {
                        policy_id: PolicyId::from_aggregate_constraint(&policy.constraint)
                            .to_string(),
                        sql,
                        invalidate_sql: Some(invalidate_sql),
                        description: policy.description.clone(),
                        constraint: policy.constraint.clone(),
                    })
                }
                _ => None,
            })
            .collect()
    }

    pub(crate) fn source_aggregate_temp_columns(
        &self,
        sink: &str,
        table_scope: Option<&TableScope>,
    ) -> Result<Vec<(SourceAggregate, String)>, RewriteError> {
        let mut temp_columns = Vec::new();
        let mut seen = HashSet::new();
        for policy in &self.policies {
            let PolicyIr::CompatAggregate(policy) = policy else {
                continue;
            };
            if policy
                .sink
                .as_deref()
                .is_some_and(|policy_sink| !policy_sink.eq_ignore_ascii_case(sink))
            {
                continue;
            }
            if let Some(table_scope) = table_scope {
                let sources_visible = policy.sources.is_empty()
                    || policy.sources.iter().all(|source| {
                        table_scope
                            .direct_base_tables
                            .contains(&TableKey::new(source))
                    });
                if !sources_visible {
                    continue;
                }
            }
            for aggregate in policy_aggregate_temp_entries(
                &policy.constraint,
                &policy.sources,
                policy.sink.as_deref(),
            )? {
                if seen.insert(aggregate.sql.clone()) {
                    temp_columns.push((aggregate, String::new()));
                }
            }
        }
        Ok(temp_columns
            .into_iter()
            .enumerate()
            .map(|(index, (aggregate, _))| (aggregate, aggregate_temp_column(index + 1)))
            .collect())
    }
}
