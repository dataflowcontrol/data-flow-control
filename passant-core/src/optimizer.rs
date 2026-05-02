use serde::{Deserialize, Serialize};

use crate::planner::ScopeInfo;
use crate::policy::{PolicyIr, Resolution};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RewriteStrategy {
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
        if scope.is_aggregation {
            candidates.push(CandidatePlan {
                strategy: RewriteStrategy::AggregateInline,
                score: 15 + scope.propagated_column_count as i32,
                reasons: vec![
                    "Query aggregates results; inline aggregate enforcement is possible".into(),
                ],
                applied_policies: policy_names.clone(),
            });
        } else {
            candidates.push(CandidatePlan {
                strategy: RewriteStrategy::RootFilter,
                score: 10 + scope.propagated_column_count as i32,
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
