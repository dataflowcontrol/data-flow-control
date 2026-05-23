use serde::{Deserialize, Serialize};

use crate::explain::{ExplainStep, RewriteExplanation};
use crate::ir::QueryIr;
use crate::optimizer::{CandidatePlan, RewriteOptimizer, RewriteStrategy};
use crate::policy::PolicyIr;
use crate::rewriter::PassantRewriter;
use crate::semiring;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ScopeInfo {
    pub visible_tables: Vec<String>,
    pub is_aggregation: bool,
    pub has_join: bool,
    pub has_outer_join: bool,
    pub has_set_operation: bool,
    pub has_non_monotonic_operation: bool,
    pub requires_source_set_annotations: bool,
    pub requires_projection_propagation: bool,
    pub propagated_column_count: usize,
    pub has_sink_mapping: bool,
    pub has_finalize_capable_sink: bool,
    pub policy_aggregate_count: usize,
    pub policy_aggregates_distributive: bool,
    pub non_distributive_policy_aggregates: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChosenPlan {
    pub strategy: RewriteStrategy,
    pub rewritten_sql: String,
    pub finalize_metadata: Vec<String>,
    pub rewrite_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanQueryResult {
    pub scope: ScopeInfo,
    pub applicable_policies: Vec<PolicyIr>,
    pub candidates: Vec<CandidatePlan>,
    pub chosen: ChosenPlan,
}

#[derive(Debug, Default)]
pub struct PassantPlanner {
    optimizer: RewriteOptimizer,
}

impl PassantPlanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn plan_query(&self, query: &QueryIr, policies: &[PolicyIr]) -> PlanQueryResult {
        let scope = self.scope_info(query, policies);
        let applicable_policies = self.matching_policies(query, policies);
        let candidate_policies = if applicable_policies.is_empty()
            && scope.has_non_monotonic_operation
            && !policies.is_empty()
        {
            policies
        } else {
            &applicable_policies
        };
        let candidates = self.optimizer.rank_candidates(&scope, candidate_policies);
        let chosen = self.choose_plan(query, &scope, &candidates, policies);

        PlanQueryResult {
            scope,
            applicable_policies,
            candidates,
            chosen,
        }
    }

    pub fn explain_rewrite(&self, query: &QueryIr, policies: &[PolicyIr]) -> RewriteExplanation {
        let result = self.plan_query(query, policies);
        let mut steps = vec![
            ExplainStep {
                stage: "parse".into(),
                detail: format!("Lowered statement into {:?}", query_variant_name(query)),
            },
            ExplainStep {
                stage: "analyze".into(),
                detail: format!(
                    "Visible tables: {}; aggregation={}; join={}; set_operation={}; non_monotonic={}; source_sets={}; policy_aggregates={}; distributive={}",
                    result.scope.visible_tables.join(", "),
                    result.scope.is_aggregation,
                    result.scope.has_join,
                    result.scope.has_set_operation,
                    result.scope.has_non_monotonic_operation,
                    result.scope.requires_source_set_annotations,
                    result.scope.policy_aggregate_count,
                    result.scope.policy_aggregates_distributive
                ),
            },
            ExplainStep {
                stage: "optimize".into(),
                detail: format!(
                    "Chose {:?} from {} candidate(s)",
                    result.chosen.strategy,
                    result.candidates.len()
                ),
            },
        ];

        if result.scope.requires_projection_propagation {
            steps.push(ExplainStep {
                stage: "propagation".into(),
                detail: format!(
                    "Planner marked {} propagated policy column(s)",
                    result.scope.propagated_column_count
                ),
            });
        }
        if let Some(error) = &result.chosen.rewrite_error {
            steps.push(ExplainStep {
                stage: "fallback".into(),
                detail: format!("Rewrite failed: {error}"),
            });
        }

        RewriteExplanation {
            scope: result.scope,
            applicable_policies: result.applicable_policies,
            candidates: result.candidates,
            chosen: result.chosen,
            steps,
        }
    }

    fn matching_policies(&self, query: &QueryIr, policies: &[PolicyIr]) -> Vec<PolicyIr> {
        let visible = visible_tables(query);
        let sink = sink_name(query);
        policies
            .iter()
            .filter(|policy| {
                let required_sources = policy
                    .required_sources()
                    .iter()
                    .map(|source| source.to_ascii_lowercase())
                    .collect::<std::collections::HashSet<_>>();
                let sources_match = policy.sources().iter().all(|source| {
                    if policy.sink().is_some()
                        && required_sources.contains(&source.to_ascii_lowercase())
                    {
                        return true;
                    }
                    visible
                        .iter()
                        .any(|table| table.eq_ignore_ascii_case(source))
                });
                let sink_match = match policy.sink() {
                    Some(policy_sink) => sink
                        .as_deref()
                        .is_some_and(|query_sink| query_sink.eq_ignore_ascii_case(policy_sink)),
                    None => true,
                };
                sources_match && sink_match
            })
            .cloned()
            .collect()
    }

    fn scope_info(&self, query: &QueryIr, policies: &[PolicyIr]) -> ScopeInfo {
        let visible = visible_tables(query);
        let propagated_column_count = policies
            .iter()
            .map(|policy| policy.constraint().matches('.').count())
            .sum();
        let semiring = semiring::analyze_policies(policies);

        let has_outer_join = raw_contains_any(
            query.raw_sql(),
            &[
                " LEFT JOIN ",
                " LEFT OUTER JOIN ",
                " RIGHT JOIN ",
                " RIGHT OUTER JOIN ",
                " FULL JOIN ",
                " FULL OUTER JOIN ",
            ],
        );
        let has_set_operation =
            raw_contains_any(query.raw_sql(), &[" UNION ", " EXCEPT ", " INTERSECT "]);
        let has_non_monotonic_operation = raw_contains_any(
            query.raw_sql(),
            &[
                " EXCEPT ",
                " FULL JOIN ",
                " FULL OUTER JOIN ",
                " ANTI JOIN ",
            ],
        );

        ScopeInfo {
            visible_tables: visible,
            is_aggregation: query_is_aggregation(query),
            has_join: raw_contains_any(query.raw_sql(), &[" JOIN "]),
            has_outer_join,
            has_set_operation,
            has_non_monotonic_operation,
            requires_source_set_annotations: has_outer_join
                || has_set_operation
                || has_non_monotonic_operation,
            requires_projection_propagation: query.raw_sql().contains("SELECT * FROM (")
                || query.raw_sql().contains("WITH "),
            propagated_column_count,
            has_sink_mapping: matches!(
                query,
                QueryIr::InsertSelect { .. } | QueryIr::Update { .. }
            ),
            has_finalize_capable_sink: sink_name(query).is_some(),
            policy_aggregate_count: semiring.aggregate_count,
            policy_aggregates_distributive: semiring.all_distributive,
            non_distributive_policy_aggregates: semiring.non_distributive_aggregates,
        }
    }

    fn choose_plan(
        &self,
        query: &QueryIr,
        scope: &ScopeInfo,
        candidates: &[CandidatePlan],
        policies: &[PolicyIr],
    ) -> ChosenPlan {
        let chosen = candidates
            .first()
            .map(|candidate| candidate.strategy)
            .unwrap_or(RewriteStrategy::CompatibilityFallback);

        let finalize_metadata = if scope.has_finalize_capable_sink {
            vec!["sink_finalize_capable".to_string()]
        } else {
            Vec::new()
        };
        let mut rewriter = PassantRewriter::new();
        for policy in policies {
            rewriter.register_policy(policy.clone());
        }
        let rewrite_result = rewriter.rewrite(query.raw_sql());
        let (rewritten_sql, rewrite_error) = match rewrite_result {
            Ok(sql) => (sql, None),
            Err(err) => (query.raw_sql().to_string(), Some(err.to_string())),
        };

        ChosenPlan {
            strategy: chosen,
            rewritten_sql,
            finalize_metadata,
            rewrite_error,
        }
    }
}

fn raw_contains_any(sql: &str, needles: &[&str]) -> bool {
    let normalized = format!(" {} ", sql.split_whitespace().collect::<Vec<_>>().join(" "));
    let upper = normalized.to_ascii_uppercase();
    needles.iter().any(|needle| upper.contains(needle))
}

fn visible_tables(query: &QueryIr) -> Vec<String> {
    match query {
        QueryIr::Select(select) => select.visible_tables(),
        QueryIr::InsertSelect { select, sink, .. } => {
            let mut tables = select.visible_tables();
            tables.push(sink.name.clone());
            tables
        }
        QueryIr::Update { sink, from, .. } => {
            let mut tables = vec![sink.alias.clone().unwrap_or_else(|| sink.name.clone())];
            for item in from {
                for table in &item.tables {
                    tables.push(table.alias.clone().unwrap_or_else(|| table.name.clone()));
                }
            }
            tables
        }
        QueryIr::Passthrough { .. } => Vec::new(),
    }
}

fn sink_name(query: &QueryIr) -> Option<String> {
    match query {
        QueryIr::InsertSelect { sink, .. } => Some(sink.name.clone()),
        QueryIr::Update { sink, .. } => Some(sink.name.clone()),
        _ => None,
    }
}

fn query_is_aggregation(query: &QueryIr) -> bool {
    match query {
        QueryIr::Select(select) => select.is_aggregation(),
        QueryIr::InsertSelect { select, .. } => select.is_aggregation(),
        _ => false,
    }
}

fn query_variant_name(query: &QueryIr) -> &'static str {
    match query {
        QueryIr::Select(_) => "select",
        QueryIr::InsertSelect { .. } => "insert_select",
        QueryIr::Update { .. } => "update",
        QueryIr::Passthrough { .. } => "passthrough",
    }
}
