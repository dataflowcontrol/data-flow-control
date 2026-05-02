use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    Remove,
    Kill,
    Invalidate,
    InvalidateMessage,
    Llm,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlowGuardPolicyKind {
    Over,
    Update,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyScope {
    pub sources: Vec<String>,
    pub sink: Option<String>,
    pub sink_alias: Option<String>,
    pub dimensions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowGuardPolicy {
    pub kind: FlowGuardPolicyKind,
    pub scope: PolicyScope,
    pub aggregations: Vec<String>,
    pub constraint: String,
    pub on_fail: Resolution,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregateDfcPolicy {
    pub sources: Vec<String>,
    pub sink: Option<String>,
    pub constraint: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyIr {
    CompatDfc {
        sources: Vec<String>,
        sink: Option<String>,
        sink_alias: Option<String>,
        constraint: String,
        on_fail: Resolution,
        description: Option<String>,
    },
    CompatAggregate(AggregateDfcPolicy),
    NativeFlowGuard(FlowGuardPolicy),
}

impl PolicyIr {
    pub fn sources(&self) -> &[String] {
        match self {
            PolicyIr::CompatDfc { sources, .. } => sources,
            PolicyIr::CompatAggregate(policy) => &policy.sources,
            PolicyIr::NativeFlowGuard(policy) => &policy.scope.sources,
        }
    }

    pub fn sink(&self) -> Option<&str> {
        match self {
            PolicyIr::CompatDfc { sink, .. } => sink.as_deref(),
            PolicyIr::CompatAggregate(policy) => policy.sink.as_deref(),
            PolicyIr::NativeFlowGuard(policy) => policy.scope.sink.as_deref(),
        }
    }

    pub fn constraint(&self) -> &str {
        match self {
            PolicyIr::CompatDfc { constraint, .. } => constraint,
            PolicyIr::CompatAggregate(policy) => &policy.constraint,
            PolicyIr::NativeFlowGuard(policy) => &policy.constraint,
        }
    }

    pub fn resolution(&self) -> Resolution {
        match self {
            PolicyIr::CompatDfc { on_fail, .. } => *on_fail,
            PolicyIr::CompatAggregate(_) => Resolution::Invalidate,
            PolicyIr::NativeFlowGuard(policy) => policy.on_fail,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            PolicyIr::CompatDfc { .. } => "compat_dfc",
            PolicyIr::CompatAggregate(_) => "compat_aggregate",
            PolicyIr::NativeFlowGuard(_) => "flowguard",
        }
    }
}
