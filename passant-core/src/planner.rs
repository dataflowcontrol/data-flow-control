use serde::{Deserialize, Serialize};

use crate::explain::{ExplainStep, RewriteExplanation};
use crate::ir::QueryIr;
use crate::optimizer::{CandidatePlan, RewriteOptimizer, RewriteStrategy};
use crate::policy::PolicyIr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeInfo {
    pub visible_tables: Vec<String>,
    pub is_aggregation: bool,
    pub requires_projection_propagation: bool,
    pub propagated_column_count: usize,
    pub has_sink_mapping: bool,
    pub has_finalize_capable_sink: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChosenPlan {
    pub strategy: RewriteStrategy,
    pub rewritten_sql: String,
    pub finalize_metadata: Vec<String>,
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
        let candidates = self.optimizer.rank_candidates(&scope, &applicable_policies);
        let chosen = self.choose_plan(query, &scope, &candidates);

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
                    "Visible tables: {}; aggregation={}",
                    result.scope.visible_tables.join(", "),
                    result.scope.is_aggregation
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

        RewriteExplanation {
            scope: result.scope,
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
                let sources_match = policy.sources().iter().all(|source| {
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

        ScopeInfo {
            visible_tables: visible,
            is_aggregation: query_is_aggregation(query),
            requires_projection_propagation: query.raw_sql().contains("SELECT * FROM (")
                || query.raw_sql().contains("WITH "),
            propagated_column_count,
            has_sink_mapping: matches!(
                query,
                QueryIr::InsertSelect { .. } | QueryIr::Update { .. }
            ),
            has_finalize_capable_sink: sink_name(query).is_some(),
        }
    }

    fn choose_plan(
        &self,
        query: &QueryIr,
        scope: &ScopeInfo,
        candidates: &[CandidatePlan],
    ) -> ChosenPlan {
        let chosen = candidates
            .first()
            .map(|candidate| candidate.strategy)
            .unwrap_or(RewriteStrategy::CompatibilityFallback);

        let prefix = match chosen {
            RewriteStrategy::RootFilter => "-- passant: root_filter",
            RewriteStrategy::ProjectionPropagation => "-- passant: projection_propagation",
            RewriteStrategy::SinkMappedRewrite => "-- passant: sink_mapped_rewrite",
            RewriteStrategy::AggregateInline => "-- passant: aggregate_inline",
            RewriteStrategy::FinalizeAggregate => "-- passant: finalize_aggregate",
            RewriteStrategy::CompatibilityFallback => "-- passant: compatibility_fallback",
        };

        let finalize_metadata = if scope.has_finalize_capable_sink {
            vec!["sink_finalize_capable".to_string()]
        } else {
            Vec::new()
        };

        ChosenPlan {
            strategy: chosen,
            rewritten_sql: format!("{prefix}\n{}", query.raw_sql()),
            finalize_metadata,
        }
    }
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
