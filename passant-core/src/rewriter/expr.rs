use crate::sql::{parse_projection_expr, wrap_table_with_filter};
use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, SelectItem, TableFactor,
};

use crate::diagnostics::RewriteError;

pub(crate) fn expr_contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function(function) => is_aggregate_name(&function.name.to_string()),
        Expr::BinaryOp { left, right, .. } => {
            expr_contains_aggregate(left) || expr_contains_aggregate(right)
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => expr_contains_aggregate(expr),
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            operand.as_deref().is_some_and(expr_contains_aggregate)
                || conditions.iter().any(expr_contains_aggregate)
                || results.iter().any(expr_contains_aggregate)
                || else_result.as_deref().is_some_and(expr_contains_aggregate)
        }
        _ => false,
    }
}

pub(crate) fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count" | "sum" | "avg" | "min" | "max" | "array_agg" | "bool_and" | "bool_or"
    )
}

pub(crate) fn parse_expr_or_identity(sql: &str) -> Expr {
    parse_expr(sql).unwrap_or_else(|_| parse_expr("true").expect("true should parse"))
}

pub(crate) fn first_function_expr(function: &sqlparser::ast::Function) -> Option<Expr> {
    let FunctionArguments::List(args) = &function.args else {
        return None;
    };
    let first = args.args.first()?;
    match first {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
        | FunctionArg::Named {
            arg: FunctionArgExpr::Expr(expr),
            ..
        }
        | FunctionArg::ExprNamed {
            arg: FunctionArgExpr::Expr(expr),
            ..
        } => Some(expr.clone()),
        _ => None,
    }
}

pub(crate) fn join_conjuncts(mut conjuncts: Vec<Expr>) -> Expr {
    let first = conjuncts.remove(0);
    conjuncts.into_iter().fold(first, and_expr)
}

pub(crate) use crate::sql::bool_literal;

pub(crate) fn table_factor_base_and_alias(
    factor: &TableFactor,
) -> Option<(String, Option<String>)> {
    match factor {
        TableFactor::Table { name, alias, .. } => Some((
            name.to_string(),
            alias.as_ref().map(|alias| alias.name.value.clone()),
        )),
        _ => None,
    }
}

pub(crate) fn projection_expr_and_name(item: &SelectItem) -> Option<(&Expr, Option<String>)> {
    match item {
        SelectItem::UnnamedExpr(expr) => Some((expr, projected_column_name(expr))),
        SelectItem::ExprWithAlias { expr, alias } => Some((expr, Some(alias.value.clone()))),
        _ => None,
    }
}

pub(crate) fn projected_column_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(ident) => Some(ident.value.clone()),
        Expr::CompoundIdentifier(parts) => parts.last().map(|ident| ident.value.clone()),
        _ => None,
    }
}

pub(crate) fn add_filter(
    select: &mut sqlparser::ast::Select,
    expr: Expr,
    is_aggregation: bool,
) -> Result<(), RewriteError> {
    let target = if is_aggregation {
        &mut select.having
    } else {
        &mut select.selection
    };
    *target = Some(match target.take() {
        Some(existing) => and_expr(existing, expr),
        None => expr,
    });
    Ok(())
}

pub(crate) fn and_expr(left: Expr, right: Expr) -> Expr {
    Expr::BinaryOp {
        left: Box::new(left),
        op: BinaryOperator::And,
        right: Box::new(right),
    }
}

pub(crate) fn parse_expr(sql: &str) -> Result<Expr, RewriteError> {
    parse_projection_expr(sql)
}

pub(crate) fn filter_table_factor(
    factor: &mut TableFactor,
    predicate: Expr,
) -> Result<(), RewriteError> {
    let TableFactor::Table { name, alias, .. } = factor else {
        return Ok(());
    };
    let alias_name = alias
        .as_ref()
        .map(|alias| alias.name.value.clone())
        .or_else(|| name.0.last().map(|part| part.value.clone()))
        .ok_or_else(|| RewriteError::unsupported_statement("table without name"))?;
    *factor = wrap_table_with_filter(factor.clone(), predicate, &alias_name);
    Ok(())
}
