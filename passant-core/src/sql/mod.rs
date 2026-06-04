//! AST-backed SQL construction helpers. Rendering happens at the rewrite boundary.

pub mod ast_stats;
pub mod builders;
pub mod columns;
pub mod dialect;
pub mod expr;
pub mod expr_key;
pub mod parse;
pub mod render;

pub use ast_stats::{count_expr, count_query, count_select, count_set_expr, count_statement};
pub use dialect::SqlDialect;
pub use expr_key::{ExprKey, expr_key_matches_str, expr_keys_equal};
pub use parse::{parse_policy_expr_duckdb, parse_projection_expr};
pub use render::{render_expr, render_object_name, render_statement};

pub use builders::{
    alias_column, alias_expr, and_exprs, binary_comparison, bool_literal, case_when,
    column_comparison, comparison_op_from_str, count_distinct_eq_one, count_distinct_ne_one, cte,
    distinct_aggregate, duckdb_array, empty_select, function_call, grouped_select, identifier,
    int_literal, is_not_null, max_column, min_column, null_literal, object_name,
    partial_push_join_from, partial_push_split_query, passant_filter_temp_column,
    passant_internal_name, passant_kill_pass_filter, qualified_column, qualified_wildcard,
    query_from_select, sanitize_projection_alias, scalar_subquery, statement_from_query,
    string_concat, string_literal, table_alias, table_factor, with_ctes, wrap_table_with_filter,
};
pub use columns::collect_qualified_columns_from_expr;
pub use expr::{rename_table_refs, replace_expr_subtrees, unqualify_table_refs};
