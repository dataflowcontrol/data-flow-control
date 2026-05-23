use serde::{Deserialize, Serialize};

use crate::optimizer::CandidatePlan;
use crate::planner::{ChosenPlan, ScopeInfo};
use crate::policy::PolicyIr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainStep {
    pub stage: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewriteExplanation {
    pub scope: ScopeInfo,
    pub applicable_policies: Vec<PolicyIr>,
    pub candidates: Vec<CandidatePlan>,
    pub chosen: ChosenPlan,
    pub steps: Vec<ExplainStep>,
}
