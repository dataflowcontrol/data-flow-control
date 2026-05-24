use sqlparser::ast::{SelectItem, SetExpr, Statement};

use crate::diagnostics::RewriteError;
use crate::parser::parse_query;

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
