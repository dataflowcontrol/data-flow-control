//! SQL output boundary for rewritten queries.
//!
//! All statement/expression text returned to callers should go through these helpers.
//! Internal rewrite planning comparisons belong in `expr_key.rs`, not ad hoc `to_string()`.

use sqlparser::ast::{Expr, ObjectName, Statement};

use super::SqlDialect;

/// Render a full SQL statement to text.
///
/// `dialect` is reserved for future dialect-specific formatting; today all backends
/// use sqlparser's generic `Display` implementation.
pub fn render_statement(statement: &Statement, dialect: Option<SqlDialect>) -> String {
    let _ = dialect;
    statement.to_string()
}

/// Render an expression to text.
///
/// Policy constraints are parsed with DuckDB-shaped syntax (`parse_policy_expr_duckdb`) even
/// when the planner dialect differs; rendered output may not match the backend until
/// dialect-aware rendering is implemented.
pub fn render_expr(expr: &Expr, dialect: Option<SqlDialect>) -> String {
    let _ = dialect;
    expr.to_string()
}

/// Render a qualified object name to text.
pub fn render_object_name(name: &ObjectName, dialect: Option<SqlDialect>) -> String {
    let _ = dialect;
    name.to_string()
}
