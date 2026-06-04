use serde::{Deserialize, Serialize};

use crate::optimizer::CandidatePlan;
use crate::planner::{ChosenPlan, ScopeInfo};
use crate::policy::PolicyIr;
use crate::rewrite_stats::RewriteStatsExport;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainStep {
    pub stage: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RewriteExplanation {
    pub scope: ScopeInfo,
    pub applicable_policies: Vec<PolicyIr>,
    pub candidates: Vec<CandidatePlan>,
    pub chosen: ChosenPlan,
    pub steps: Vec<ExplainStep>,
    /// Policy candidate/applicable/dominated counts when available from indexed planning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_plan: Option<crate::rewriter::ScopePlanDiagnostics>,
    /// Per-scope planning diagnostics when a full statement plan is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement_plan: Option<crate::rewriter::StatementRewriteSummary>,
    /// Rewrite counters/timings from the most recent rewrite when stats were collected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewrite_stats: Option<RewriteStatsExport>,
}
