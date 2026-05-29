pub mod catalog;
pub mod diagnostics;
pub mod explain;
pub mod full_push;
pub mod identifiers;
pub mod intern;
pub mod ir;
pub mod optimizer;
pub mod parser;
pub mod partial_push;
pub mod planner;
pub mod policy;
pub mod policy_index;
pub mod policy_store;
pub mod query_analysis;
pub mod rewrite_stats;
pub mod rewrite_strategy;
pub mod rewriter;
pub mod semiring;
pub mod source_set_index;
pub mod source_sets;
pub mod sql;
pub mod statement_tables;
pub mod threshold;

pub use catalog::{
    CatalogSnapshot, CatalogTableInfo, TableCatalog, validate_constraint_expression,
};
pub use diagnostics::{ErrorKind, RewriteError};
pub use explain::{ExplainStep, RewriteExplanation};
pub use identifiers::{
    Alias, AliasByBase, ColumnKey, ColumnName, PolicyId, QualifiedColumn, SinkName, SourceName,
    SqlIdent, TableKey, TableName, column_name_from_expr, table_name_from_column_expr,
};
pub use intern::StringInterner;
pub use ir::{
    Assignment, ExprRef, FromItem, JoinRef, PassantSelect, ProjectionItem, QueryIr, TableRef,
};
pub use optimizer::{CandidatePlan, RewriteOptimizer, RewriteStrategy};
pub use parser::{
    ParseArtifact, ParseError, parse_query, parse_query_to_ir, parse_query_with_dialect,
};
pub use planner::{ChosenPlan, PassantPlanner, PlanQueryResult, ScopeInfo};
pub use policy::{
    PolicyIr, PolicyParseError, Resolution, normalize_policy_dimension_aliases,
    normalize_policy_dimension_queries, normalize_policy_dimensions,
    normalize_policy_source_aliases, normalize_policy_sources, parse_policy_text,
};
pub use policy_store::{
    CompiledExpr, CompiledPolicy, MultiSourceLookupMode, PolicyStore, PolicyStoreMemoryUsage,
};
pub use query_analysis::{SelectAnalysis, StatementAnalysis};
pub use rewrite_stats::{RewriteStats, RewriteStatsExport, RewriteStatsTimings};
pub use rewrite_strategy::{
    RewriteAttempt, RewriteEngine, RewritePipeline, RewriteRequest, SelectShape, StatementKind,
    StrategyKind,
};
pub use rewriter::{
    PassantRewriter, RewriteOptions, ScopePlanDiagnostics, SelectRewritePlan,
    StatementRewriteSummary, plan_statement_rewrite_summary,
};
pub use semiring::{AggregateAnalysis, SemiringAnalysis, analyze_constraint};
pub use source_sets::{
    cross_source_policies_for_branch, policy_requires_set_split, select_has_anti_join,
    select_has_full_join, select_nullable_source_tables, select_source_tables,
    set_expr_source_tables, set_operation_requires_cross_source_policies,
    split_policy_by_source_local_conjuncts, split_policy_for_set_branches,
    split_select_policies_for_nullable_joins, split_select_policies_for_nullable_joins_for_store,
    split_set_operation_policies, table_factor_source_tables,
};
pub use sql::SqlDialect;
pub use statement_tables::{statement_sink_key, statement_table_keys};
pub use threshold::{prune_dominated_remove_policies, threshold_dominates};
