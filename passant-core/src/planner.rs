use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::aggregate_registry::AggregateRegistry;
use crate::explain::{ExplainStep, RewriteExplanation};
use crate::identifiers::TableKey;
use crate::ir::QueryIr;
use crate::optimizer::{CandidatePlan, RewriteOptimizer, RewriteStrategy};
use crate::policy::PolicyIr;
use crate::rewriter::{PassantRewriter, resolve_scope_policies};
use crate::semiring;
use crate::sql::SqlDialect;
use crate::threshold;

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
    /// Human-readable reasons the chosen strategy was selected.
    #[serde(default)]
    pub strategy_reasons: Vec<String>,
    /// Strategies considered but not selected (strategy + reason).
    #[serde(default)]
    pub skipped_strategies: Vec<String>,
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
        self.plan_query_with_registry(
            query,
            policies,
            &AggregateRegistry::for_dialect(SqlDialect::DuckDb),
        )
    }

    pub fn plan_query_with_registry(
        &self,
        query: &QueryIr,
        policies: &[PolicyIr],
        registry: &AggregateRegistry,
    ) -> PlanQueryResult {
        self.plan_query_in_context(query, policies, registry, None, false)
    }

    pub fn plan_query_with_rewriter(
        &self,
        query: &QueryIr,
        rewriter: &PassantRewriter,
    ) -> PlanQueryResult {
        let context = rewriter.clone();
        self.plan_query_in_context(
            query,
            &context.policies(),
            context.aggregate_registry(),
            Some(&context),
            false,
        )
    }

    fn plan_query_in_context(
        &self,
        query: &QueryIr,
        policies: &[PolicyIr],
        registry: &AggregateRegistry,
        rewriter: Option<&PassantRewriter>,
        collect_stats: bool,
    ) -> PlanQueryResult {
        let scope = self.scope_info(query, policies, registry);
        let applicable_policies =
            threshold::prune_dominated_remove_policies(&self.matching_policies(query, policies));
        let candidate_policies = if applicable_policies.is_empty()
            && scope.has_non_monotonic_operation
            && !policies.is_empty()
        {
            policies
        } else {
            &applicable_policies
        };
        let candidates = self.optimizer.rank_candidates(&scope, candidate_policies);
        let chosen = self.choose_plan(
            query,
            &scope,
            &candidates,
            policies,
            rewriter,
            collect_stats,
        );

        PlanQueryResult {
            scope,
            applicable_policies,
            candidates,
            chosen,
        }
    }

    pub fn explain_rewrite(&self, query: &QueryIr, policies: &[PolicyIr]) -> RewriteExplanation {
        self.explain_rewrite_with_registry(
            query,
            policies,
            &AggregateRegistry::for_dialect(SqlDialect::DuckDb),
        )
    }

    pub fn explain_rewrite_with_registry(
        &self,
        query: &QueryIr,
        policies: &[PolicyIr],
        registry: &AggregateRegistry,
    ) -> RewriteExplanation {
        self.explain_in_context(query, policies, registry, None, false)
    }

    /// Explain the chosen rewrite with the complete registered catalog and
    /// function/policy metadata. Clone cells isolate diagnostics from the caller;
    /// the chosen SQL and optional statistics come from one compiler invocation.
    pub fn explain_rewrite_with_rewriter(
        &self,
        query: &QueryIr,
        rewriter: &PassantRewriter,
        collect_stats: bool,
    ) -> RewriteExplanation {
        let context = rewriter.clone();
        self.explain_in_context(
            query,
            &context.policies(),
            context.aggregate_registry(),
            Some(&context),
            collect_stats,
        )
    }

    fn explain_in_context(
        &self,
        query: &QueryIr,
        policies: &[PolicyIr],
        registry: &AggregateRegistry,
        registered: Option<&PassantRewriter>,
        collect_stats: bool,
    ) -> RewriteExplanation {
        let result =
            self.plan_query_in_context(query, policies, registry, registered, collect_stats);
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
                    "Chose {:?} from {} candidate(s): {}",
                    result.chosen.strategy,
                    result.candidates.len(),
                    result.chosen.strategy_reasons.join("; ")
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

        let fallback = if registered.is_none() {
            PassantRewriter::from_policies(policies.to_vec()).ok()
        } else {
            None
        };
        let policy_plan = registered.or(fallback.as_ref()).map(|rewriter| {
            let visible_keys = result
                .scope
                .visible_tables
                .iter()
                .map(|table| TableKey::new(table))
                .collect::<HashSet<_>>();
            let (_, diagnostics) = resolve_scope_policies(
                rewriter.policy_store(),
                &visible_keys,
                None,
                false,
                &HashSet::new(),
                &HashSet::new(),
            );
            steps.push(ExplainStep {
                stage: "policy_plan".into(),
                detail: format!(
                    "total={}; candidates={}; applicable={}; dominated={}",
                    rewriter.policy_store().active_count(),
                    diagnostics.candidate_policies,
                    diagnostics.applicable_policies,
                    diagnostics.dominated_policies
                ),
            });
            diagnostics
        });

        let statement_plan = registered.and_then(|rewriter| {
            crate::parser::parse_query_with_dialect(query.raw_sql(), rewriter.sql_dialect())
                .ok()
                .map(|statement| rewriter.plan_statement_summary(&statement))
        });
        let policy_plan = statement_plan
            .as_ref()
            .map(|plan| plan.aggregate())
            .or(policy_plan);
        if let Some(plan) = &policy_plan
            && let Some(step) = steps.iter_mut().find(|step| step.stage == "policy_plan")
        {
            step.detail = format!(
                "candidates={}; applicable={}; dominated={}",
                plan.candidate_policies, plan.applicable_policies, plan.dominated_policies
            );
        }
        let rewrite_stats = if collect_stats {
            registered.map(|rewriter| rewriter.last_rewrite_stats().into())
        } else {
            None
        };
        RewriteExplanation {
            scope: result.scope,
            applicable_policies: result.applicable_policies,
            candidates: result.candidates,
            chosen: result.chosen,
            steps,
            policy_plan,
            statement_plan,
            rewrite_stats,
        }
    }

    fn matching_policies(&self, query: &QueryIr, policies: &[PolicyIr]) -> Vec<PolicyIr> {
        let visible = visible_tables(query);
        let base_tables = query
            .direct_base_tables()
            .into_iter()
            .map(|table| TableKey::new(&table))
            .collect::<std::collections::HashSet<_>>();
        let sink = sink_name(query);
        policies
            .iter()
            .filter(|policy| {
                let required_sources = policy
                    .required_sources()
                    .iter()
                    .map(|source| TableKey::new(source))
                    .collect::<std::collections::HashSet<_>>();
                let sources_match = policy.sources().iter().all(|source| {
                    let source_key = TableKey::new(source);
                    if policy.sink().is_some() && required_sources.contains(&source_key) {
                        return true;
                    }
                    base_tables.contains(&source_key)
                        || visible
                            .iter()
                            .any(|table| TableKey::new(table) == source_key)
                });
                let sink_match = match policy.sink() {
                    Some(policy_sink) => sink.as_deref().is_some_and(|query_sink| {
                        TableKey::new(query_sink) == TableKey::new(policy_sink)
                    }),
                    None => true,
                };
                sources_match && sink_match
            })
            .cloned()
            .collect()
    }

    fn scope_info(
        &self,
        query: &QueryIr,
        policies: &[PolicyIr],
        registry: &AggregateRegistry,
    ) -> ScopeInfo {
        let visible = visible_tables(query);
        let propagated_column_count = policies
            .iter()
            .map(|policy| policy.constraint().matches('.').count())
            .sum();
        let semiring = semiring::analyze_policies_with_registry(policies, registry);

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
        registered: Option<&PassantRewriter>,
        collect_stats: bool,
    ) -> ChosenPlan {
        let chosen_candidate = candidates.first();
        let chosen = chosen_candidate
            .map(|candidate| candidate.strategy)
            .unwrap_or(RewriteStrategy::StableOutputFallback);
        let strategy_reasons = chosen_candidate
            .map(|candidate| candidate.reasons.clone())
            .unwrap_or_default();
        let skipped_strategies = candidates
            .iter()
            .skip(1)
            .take(3)
            .map(|candidate| format!("{:?}: {}", candidate.strategy, candidate.reasons.join("; ")))
            .collect();

        let finalize_metadata = if scope.has_finalize_capable_sink {
            vec!["sink_finalize_capable".to_string()]
        } else {
            Vec::new()
        };
        let rewrite_options = crate::rewriter::RewriteOptions {
            use_partial_push: chosen == crate::optimizer::RewriteStrategy::PartialPush,
            collect_stats,
            ..Default::default()
        };
        let rewrite_result = if let Some(registered) = registered {
            registered.rewrite_with_options(query.raw_sql(), rewrite_options)
        } else {
            PassantRewriter::from_policies(policies.to_vec()).and_then(|rewriter| {
                rewriter.rewrite_with_options(query.raw_sql(), rewrite_options)
            })
        };
        let (rewritten_sql, rewrite_error) = match rewrite_result {
            Ok(sql) => (sql, None),
            Err(err) => (query.raw_sql().to_string(), Some(err.to_string())),
        };

        ChosenPlan {
            strategy: chosen,
            rewritten_sql,
            finalize_metadata,
            rewrite_error,
            strategy_reasons,
            skipped_strategies,
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
