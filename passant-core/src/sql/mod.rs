//! AST-backed SQL construction helpers. Rendering happens at the rewrite boundary.

pub mod builders;
pub mod expr;
pub mod parse;

pub use parse::parse_projection_expr;

pub use builders::{
    alias_column, alias_expr, and_exprs, binary_comparison, bool_literal, case_when,
    column_comparison, comparison_op_from_str, count_distinct_eq_one, cte, distinct_aggregate,
    duckdb_array, empty_select, function_call, grouped_select, identifier, int_literal,
    is_not_null, null_literal, object_name, or_kill, partial_push_join_from,
    partial_push_split_query, passant_agg_temp_column, passant_filter_temp_column,
    passant_internal_name, qualified_column, qualified_wildcard, query_from_select,
    sanitize_projection_alias, scalar_subquery, statement_from_query, string_concat,
    string_literal, table_alias, table_factor, with_ctes, wrap_table_with_filter,
};
pub use expr::{rename_table_refs, replace_expr_subtrees, unqualify_table_refs};
