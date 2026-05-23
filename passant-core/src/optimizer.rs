use serde::{Deserialize, Serialize};

use crate::planner::ScopeInfo;
use crate::policy::{PolicyIr, Resolution};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RewriteStrategy {
    FullPush,
    PartialPush,
    LogicalFallback,
    RootFilter,
    ProjectionPropagation,
    SinkMappedRewrite,
    AggregateInline,
    FinalizeAggregate,
    CompatibilityFallback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidatePlan {
    pub strategy: RewriteStrategy,
    pub score: i32,
    pub reasons: Vec<String>,
    pub applied_policies: Vec<String>,
}

#[derive(Debug, Default)]
pub struct RewriteOptimizer;

impl RewriteOptimizer {
    pub fn rank_candidates(&self, scope: &ScopeInfo, policies: &[PolicyIr]) -> Vec<CandidatePlan> {
        let mut candidates = Vec::new();

        if policies.is_empty() {
            candidates.push(CandidatePlan {
                strategy: RewriteStrategy::CompatibilityFallback,
                score: 0,
                reasons: vec!["No applicable policies".to_string()],
                applied_policies: Vec::new(),
            });
            return candidates;
        }

        let policy_names = policies
            .iter()
            .map(|p| p.name().to_string())
            .collect::<Vec<_>>();
        if scope.has_non_monotonic_operation {
            candidates.push(CandidatePlan {
                strategy: RewriteStrategy::LogicalFallback,
                score: 5,
                reasons: vec![
                    "Query contains a non-monotonic construct that requires source-set semantics or a logical fallback".into(),
                ],
                applied_policies: policy_names.clone(),
            });
        } else if !scope.policy_aggregates_distributive {
            candidates.push(CandidatePlan {
                strategy: RewriteStrategy::PartialPush,
                score: 5,
                reasons: vec![format!(
                    "Policy uses non-distributive aggregate(s): {}",
                    scope.non_distributive_policy_aggregates.join(", ")
                )],
                applied_policies: policy_names.clone(),
            });
        } else if scope.is_aggregation || scope.has_outer_join || scope.has_sink_mapping {
            let reason = if scope.requires_source_set_annotations {
                "Policy enforcement needs source-set-aware propagation before final aggregation, outer join, or sink mapping"
            } else {
                "Policy enforcement needs boundary-aware propagation before final aggregation, outer join, or sink mapping"
            };
            candidates.push(CandidatePlan {
                strategy: RewriteStrategy::PartialPush,
                score: 5,
                reasons: vec![reason.into()],
                applied_policies: policy_names.clone(),
            });
        } else {
            candidates.push(CandidatePlan {
                strategy: RewriteStrategy::FullPush,
                score: 5,
                reasons: vec![
                    "Query is monotonic in the supported SPJU fragment; policy predicates can be pushed to contributing tuples".into(),
                ],
                applied_policies: policy_names.clone(),
            });
        }

        if scope.is_aggregation {
            candidates.push(CandidatePlan {
                strategy: RewriteStrategy::AggregateInline,
                score: 25 + scope.propagated_column_count as i32,
                reasons: vec![
                    "Query aggregates results; inline aggregate enforcement is possible".into(),
                ],
                applied_policies: policy_names.clone(),
            });
        } else {
            candidates.push(CandidatePlan {
                strategy: RewriteStrategy::RootFilter,
                score: 20 + scope.propagated_column_count as i32,
                reasons: vec!["Root-local filtering preserves original query shape".into()],
                applied_policies: policy_names.clone(),
            });
        }

        if scope.requires_projection_propagation {
            candidates.push(CandidatePlan {
                strategy: RewriteStrategy::ProjectionPropagation,
                score: 40 + scope.propagated_column_count as i32 * 5,
                reasons: vec![
                    "Missing policy inputs must be exposed through a subquery or CTE".into(),
                ],
                applied_policies: policy_names.clone(),
            });
        }

        if scope.has_sink_mapping {
            candidates.push(CandidatePlan {
                strategy: RewriteStrategy::SinkMappedRewrite,
                score: 20,
                reasons: vec!["Sink-side references can be remapped to output assignments".into()],
                applied_policies: policy_names.clone(),
            });
        }

        if policies
            .iter()
            .any(|policy| matches!(policy.resolution(), Resolution::Invalidate))
            && scope.has_finalize_capable_sink
        {
            candidates.push(CandidatePlan {
                strategy: RewriteStrategy::FinalizeAggregate,
                score: 35,
                reasons: vec!["Aggregate invalidation can be deferred to finalize metadata".into()],
                applied_policies: policy_names.clone(),
            });
        }

        candidates.push(CandidatePlan {
            strategy: RewriteStrategy::CompatibilityFallback,
            score: 100,
            reasons: vec!["Legacy-compatible fallback preserves output stability".into()],
            applied_policies: policy_names,
        });

        candidates.sort_by_key(|candidate| candidate.score);
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::ScopeInfo;
    use crate::policy::{PolicyIr, Resolution};

    fn empty_scope() -> ScopeInfo {
        ScopeInfo::default()
    }

    fn sample_policy() -> PolicyIr {
        PolicyIr::CompatDfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(foo.id) > 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }
    }

    #[test]
    fn empty_policies_return_compatibility_fallback() {
        let candidates = RewriteOptimizer.rank_candidates(&empty_scope(), &[]);
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].strategy,
            RewriteStrategy::CompatibilityFallback
        );
    }

    #[test]
    fn non_monotonic_scope_prefers_logical_fallback() {
        let mut scope = empty_scope();
        scope.has_non_monotonic_operation = true;
        let candidates = RewriteOptimizer.rank_candidates(&scope, &[sample_policy()]);
        assert_eq!(candidates[0].strategy, RewriteStrategy::LogicalFallback);
    }

    #[test]
    fn monotonic_spj_scope_includes_full_push_candidate() {
        let mut scope = empty_scope();
        scope.policy_aggregates_distributive = true;
        let candidates = RewriteOptimizer.rank_candidates(&scope, &[sample_policy()]);
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.strategy == RewriteStrategy::FullPush)
        );
    }
}
