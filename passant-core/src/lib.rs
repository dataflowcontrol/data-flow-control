pub mod catalog;
pub mod diagnostics;
pub mod explain;
pub mod full_push;
pub mod identifiers;
pub mod ir;
pub mod optimizer;
pub mod parser;
pub mod partial_push;
pub mod planner;
pub mod policy;
pub mod rewrite_strategy;
pub mod rewriter;
pub mod semiring;
pub mod source_sets;
pub mod sql;
pub mod threshold;

pub use catalog::{
    CatalogSnapshot, CatalogTableInfo, TableCatalog, validate_constraint_expression,
};
pub use diagnostics::{ErrorKind, RewriteError};
pub use explain::{ExplainStep, RewriteExplanation};
pub use identifiers::{
    Alias, AliasByBase, ColumnName, PolicyId, QualifiedColumn, SinkName, SourceName, SqlIdent,
    TableKey, TableName, column_name_from_expr, table_name_from_column_expr,
};
pub use ir::{
    Assignment, ExprRef, FromItem, JoinRef, PassantSelect, ProjectionItem, QueryIr, TableRef,
};
pub use optimizer::{CandidatePlan, RewriteOptimizer, RewriteStrategy};
pub use parser::{ParseArtifact, ParseError, parse_query, parse_query_to_ir};
pub use planner::{ChosenPlan, PassantPlanner, PlanQueryResult, ScopeInfo};
pub use policy::{
    AggregateDfcPolicy, PgnPolicy, PgnPolicyKind, PolicyIr, PolicyParseError, PolicyScope,
    Resolution, normalize_policy_dimensions, normalize_policy_sources, parse_policy_text,
};
pub use rewrite_strategy::{
    RewriteAttempt, RewriteEngine, RewritePipeline, RewriteRequest, SelectShape, StatementKind,
    StrategyKind,
};
pub use rewriter::{PassantRewriter, RewriteOptions};
pub use semiring::{AggregateAnalysis, SemiringAnalysis, analyze_constraint};
pub use source_sets::{
    cross_source_policies_for_branch, policy_requires_set_split, select_has_anti_join,
    select_has_full_join, select_nullable_source_tables, select_source_tables,
    set_expr_source_tables, set_operation_requires_cross_source_policies,
    split_policy_by_source_local_conjuncts, split_policy_for_set_branches,
    split_select_policies_for_nullable_joins, split_set_operation_policies,
    table_factor_source_tables,
};
pub use threshold::{prune_dominated_remove_policies, threshold_dominates};
