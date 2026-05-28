use sqlparser::ast::{SelectItem, SetExpr, Statement};
use sqlparser::dialect::DuckDbDialect;
use sqlparser::parser::Parser;

use crate::diagnostics::RewriteError;
use crate::parser::parse_query;

/// Parse a policy constraint or dimension expression using DuckDB SQL syntax.
pub fn parse_policy_expr_duckdb(sql: &str) -> Result<sqlparser::ast::Expr, RewriteError> {
    let mut statements = Parser::parse_sql(&DuckDbDialect {}, &format!("SELECT {sql}"))
        .map_err(|err| RewriteError::unsupported_statement(format!("policy expression: {err}")))?;
    let statement = statements
        .pop()
        .ok_or_else(|| RewriteError::unsupported_statement("empty policy expression"))?;
    let Statement::Query(query) = statement else {
        return Err(RewriteError::unsupported_statement("policy expression"));
    };
    let SetExpr::Select(select) = *query.body else {
        return Err(RewriteError::unsupported_statement("policy expression"));
    };
    let Some(item) = select.projection.into_iter().next() else {
        return Err(RewriteError::unsupported_statement(
            "empty policy expression",
        ));
    };
    match item {
        SelectItem::UnnamedExpr(expr) => Ok(expr),
        SelectItem::ExprWithAlias { expr, .. } => Ok(expr),
        other => Err(RewriteError::unsupported_statement(format!(
            "policy projection {other}"
        ))),
    }
}

/// Parse a single projection expression from SQL text.
pub fn parse_projection_expr(sql: &str) -> Result<sqlparser::ast::Expr, RewriteError> {
    let statement = parse_query(&format!("SELECT {sql}"))?;
    let Statement::Query(query) = statement else {
        return Err(RewriteError::unsupported_statement("constraint expression"));
    };
    let SetExpr::Select(select) = *query.body else {
        return Err(RewriteError::unsupported_statement("constraint expression"));
    };
    let Some(item) = select.projection.into_iter().next() else {
        return Err(RewriteError::unsupported_statement(
            "empty constraint expression",
        ));
    };
    match item {
        SelectItem::UnnamedExpr(expr) => Ok(expr),
        SelectItem::ExprWithAlias { expr, .. } => Ok(expr),
        other => Err(RewriteError::unsupported_statement(format!(
            "constraint projection {other}"
        ))),
    }
}
