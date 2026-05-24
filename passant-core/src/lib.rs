pub mod explain;
pub mod ir;
pub mod optimizer;
pub mod parser;
pub mod planner;
pub mod policy;
pub mod rewriter;
pub mod semiring;
pub mod source_sets;
pub mod threshold;

pub use explain::{ExplainStep, RewriteExplanation};
pub use ir::{
    Assignment, ExprRef, FromItem, JoinRef, PassantSelect, ProjectionItem, QueryIr, TableRef,
};
pub use optimizer::{CandidatePlan, RewriteOptimizer, RewriteStrategy};
pub use parser::{ParseArtifact, ParseError, parse_query, parse_query_to_ir};
pub use planner::{ChosenPlan, PassantPlanner, PlanQueryResult, ScopeInfo};
pub use policy::{
    AggregateDfcPolicy, PgnPolicy, PgnPolicyKind, PolicyIr, PolicyParseError,
    PolicyScope, Resolution, parse_policy_text,
};
pub use rewriter::{PassantRewriter, RewriteError, TableCatalog};
pub use semiring::{AggregateAnalysis, SemiringAnalysis, analyze_constraint};
pub use source_sets::{
    cross_source_policies_for_branch, policy_requires_set_split,
    select_has_anti_join, select_has_full_join, select_nullable_source_tables,
    select_source_tables, set_expr_source_tables, set_operation_requires_cross_source_policies,
    split_policy_by_source_local_conjuncts, split_policy_for_set_branches,
    split_select_policies_for_nullable_joins, split_set_operation_policies,
    table_factor_source_tables,
};
pub use threshold::{prune_dominated_remove_policies, threshold_dominates};
