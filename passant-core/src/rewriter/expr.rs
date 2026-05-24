use crate::sql::{
    binary_comparison, case_when, function_call, null_literal, or_kill, parse_projection_expr,
    qualified_column, string_concat, string_literal, wrap_table_with_filter,
};
use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr, Ident,
    Select, SelectItem, TableFactor,
};

use crate::diagnostics::RewriteError;
use crate::policy::Resolution;

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

pub(crate) fn apply_resolution(
    select: &mut Select,
    expr: Expr,
    resolution: Resolution,
    description: Option<&str>,
    is_aggregation: bool,
    projection_expr: Option<Expr>,
) -> Result<(), RewriteError> {
    let invalidate_expr = projection_expr.unwrap_or_else(|| expr.clone());
    match resolution {
        Resolution::Remove => add_filter(select, expr, is_aggregation),
        Resolution::Kill => add_filter(
            select,
            kill_expr_for_select(expr, select, is_aggregation)?,
            is_aggregation,
        ),
        Resolution::Invalidate => {
            upsert_select_projection(select, "valid", |existing| {
                Ok::<_, RewriteError>(existing.map_or(invalidate_expr.clone(), |existing| {
                    and_expr(existing, invalidate_expr.clone())
                }))
            })?;
            Ok(())
        }
        Resolution::InvalidateMessage => {
            upsert_select_projection(select, "invalid_string", |existing| {
                if let Some(existing) = existing {
                    append_invalid_message_expr(existing, expr.clone(), description)
                } else {
                    invalidate_message_expr(expr.clone(), description)
                }
            })?;
            Ok(())
        }
        Resolution::Llm => add_filter(select, resolver_expr(expr)?, is_aggregation),
    }
}

fn upsert_select_projection<F, E>(select: &mut Select, name: &str, build_expr: F) -> Result<(), E>
where
    F: FnOnce(Option<Expr>) -> Result<Expr, E>,
{
    if let Some(position) = select.projection.iter().position(|item| {
        projection_expr_and_name(item)
            .and_then(|(_, alias)| alias)
            .is_some_and(|alias| alias.eq_ignore_ascii_case(name))
    }) {
        let existing =
            projection_expr_and_name(&select.projection[position]).map(|(expr, _)| expr.clone());
        select.projection[position] = SelectItem::ExprWithAlias {
            expr: build_expr(existing)?,
            alias: Ident::new(name),
        };
        return Ok(());
    }

    select.projection.push(SelectItem::ExprWithAlias {
        expr: build_expr(None)?,
        alias: Ident::new(name),
    });
    Ok(())
}

pub(crate) fn add_filter(
    select: &mut Select,
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

pub(crate) fn kill_expr(expr: Expr) -> Result<Expr, RewriteError> {
    Ok(or_kill(expr))
}

fn kill_expr_for_select(
    expr: Expr,
    select: &Select,
    is_aggregation: bool,
) -> Result<Expr, RewriteError> {
    if !is_aggregation {
        return kill_expr(expr);
    }
    let tautology = aggregation_kill_tautology(select)?;
    Ok(case_when(expr, or_kill(tautology), bool_literal(true)))
}

fn aggregation_kill_tautology(select: &Select) -> Result<Expr, RewriteError> {
    let source_prefix = select
        .from
        .first()
        .and_then(|table| table_factor_base_and_alias(&table.relation))
        .map(|(base, _)| base);
    if let GroupByExpr::Expressions(exprs, _) = &select.group_by
        && let Some(group_expr) = exprs.first()
    {
        let tautology = qualify_kill_tautology_expr(group_expr, source_prefix.as_deref());
        return Ok(binary_comparison(
            tautology.clone(),
            BinaryOperator::Eq,
            tautology,
        ));
    }
    for item in &select.projection {
        if let SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } = item
            && !expr_contains_aggregate(expr)
        {
            let tautology = qualify_kill_tautology_expr(expr, source_prefix.as_deref());
            return Ok(binary_comparison(
                tautology.clone(),
                BinaryOperator::Eq,
                tautology,
            ));
        }
    }
    Ok(bool_literal(true))
}

fn qualify_kill_tautology_expr(expr: &Expr, source_prefix: Option<&str>) -> Expr {
    if let (Some(prefix), Expr::Identifier(ident)) = (source_prefix, expr) {
        return qualified_column(prefix, &ident.value);
    }
    expr.clone()
}

pub(crate) fn resolver_expr(expr: Expr) -> Result<Expr, RewriteError> {
    Ok(case_when(
        expr,
        bool_literal(true),
        function_call("address_violating_rows", Vec::new()),
    ))
}

pub(crate) fn invalidate_message_expr(
    expr: Expr,
    description: Option<&str>,
) -> Result<Expr, RewriteError> {
    let message = description.unwrap_or("DFC policy violation");
    Ok(case_when(expr, null_literal(), string_literal(message)))
}

pub(crate) fn append_invalid_message_expr(
    existing: Expr,
    expr: Expr,
    description: Option<&str>,
) -> Result<Expr, RewriteError> {
    let message = description.unwrap_or("DFC policy violation");
    let else_branch = string_concat(
        function_call(
            "COALESCE",
            vec![
                string_concat(existing.clone(), string_literal("; ")),
                string_literal(""),
            ],
        ),
        string_literal(message),
    );
    Ok(case_when(expr, existing, else_branch))
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
