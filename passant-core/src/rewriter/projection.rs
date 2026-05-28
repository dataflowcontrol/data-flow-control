use std::collections::HashSet;

use sqlparser::ast::{BinaryOperator, Expr, GroupByExpr, Ident, Select, SelectItem};

use crate::diagnostics::RewriteError;
use crate::sql::ExprKey;

use super::PassantRewriter;
use super::expr::{expr_contains_aggregate, projected_column_name, projection_expr_and_name};
use super::plan::{apply_policy_resolution_actions, plan_policy_filter_actions};
use super::scope::TableScope;
use super::types::RewriteContext;
use crate::sql::sanitize_projection_alias;

pub(crate) fn outer_limited_projection_items(select: &Select) -> Vec<SelectItem> {
    select
        .projection
        .iter()
        .filter(|item| !is_passant_filter_projection(item))
        .map(|item| match item {
            SelectItem::UnnamedExpr(expr) => SelectItem::UnnamedExpr(Expr::Identifier(Ident::new(
                projected_column_name(expr).unwrap_or_else(|| crate::sql::render_expr(expr, None)),
            ))),
            SelectItem::ExprWithAlias { alias, .. } => {
                SelectItem::UnnamedExpr(Expr::Identifier(alias.clone()))
            }
            other => SelectItem::UnnamedExpr(Expr::Identifier(Ident::new(other.to_string()))),
        })
        .collect()
}

fn is_passant_filter_projection(item: &SelectItem) -> bool {
    matches!(
        item,
        SelectItem::ExprWithAlias { alias, .. }
            if alias.value.starts_with("__passant_filter_")
    )
}

fn auto_alias_for_expression(expr: &Expr) -> String {
    sanitize_projection_alias(&crate::sql::render_expr(expr, None))
}

pub(crate) fn ensure_projection_aliases(select: &mut Select) {
    let mut projection = Vec::with_capacity(select.projection.len());
    for item in std::mem::take(&mut select.projection) {
        match item {
            SelectItem::UnnamedExpr(expr) if projected_column_name(&expr).is_none() => {
                let alias = auto_alias_for_expression(&expr);
                projection.push(SelectItem::ExprWithAlias {
                    expr,
                    alias: Ident::new(&alias),
                });
            }
            other => projection.push(other),
        }
    }
    select.projection = projection;
}

trait GroupByEmpty {
    fn is_empty(&self) -> bool;
}

impl GroupByEmpty for GroupByExpr {
    fn is_empty(&self) -> bool {
        match self {
            GroupByExpr::All(_) => false,
            GroupByExpr::Expressions(exprs, _) => exprs.is_empty(),
        }
    }
}

pub(crate) fn select_is_aggregation(select: &Select) -> bool {
    !select.group_by.is_empty()
        || select.having.is_some()
        || select.projection.iter().any(select_item_contains_aggregate)
}

fn select_item_contains_aggregate(item: &SelectItem) -> bool {
    match item {
        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
            expr_contains_aggregate(expr)
        }
        _ => false,
    }
}

pub(crate) fn group_by_join_specs(select: &Select) -> Result<Vec<(String, Expr)>, RewriteError> {
    let GroupByExpr::Expressions(group_exprs, _) = &select.group_by else {
        return Ok(Vec::new());
    };
    if group_exprs.is_empty() {
        return Ok(Vec::new());
    }

    let mut specs = Vec::new();
    for group_expr in group_exprs {
        let group_key = ExprKey::from_expr(group_expr);
        let mut matched = None;
        for item in &select.projection {
            let Some((expr, alias)) = projection_expr_and_name(item) else {
                continue;
            };
            if ExprKey::from_expr(expr) == group_key {
                let key = alias.unwrap_or_else(|| crate::sql::render_expr(expr, None));
                matched = Some((key, expr.clone()));
                break;
            }
        }
        if matched.is_none()
            && let Expr::Identifier(ident) = group_expr
        {
            for item in &select.projection {
                let Some((expr, alias)) = projection_expr_and_name(item) else {
                    continue;
                };
                if alias.is_some_and(|alias| alias.eq_ignore_ascii_case(&ident.value))
                    || projected_column_name(expr)
                        .is_some_and(|name| name.eq_ignore_ascii_case(&ident.value))
                {
                    matched = Some((ident.value.clone(), expr.clone()));
                    break;
                }
            }
        }
        let Some((key, expr)) = matched else {
            return Err(RewriteError::unsupported_statement(
                "partial-push requires GROUP BY expressions to be projected in SELECT with stable output names",
            ));
        };
        specs.push((key, expr));
    }
    Ok(specs)
}

pub(crate) fn extract_policy_comparison_from_expr(
    expr: &Expr,
) -> Result<(String, String, &'static str), RewriteError> {
    let (left, op, right) = match expr {
        Expr::BinaryOp { left, op, right } => (left.as_ref(), op, right.as_ref()),
        _ => {
            return Err(RewriteError::unsupported_statement(
                "partial-push policy constraint must be a comparison",
            ));
        }
    };
    let op_str = match op {
        BinaryOperator::Gt => ">",
        BinaryOperator::GtEq => ">=",
        BinaryOperator::Lt => "<",
        BinaryOperator::LtEq => "<=",
        BinaryOperator::Eq => "=",
        BinaryOperator::NotEq => "!=",
        _ => {
            return Err(RewriteError::unsupported_statement(
                "partial-push policy constraint uses unsupported comparison operator",
            ));
        }
    };
    Ok((
        crate::sql::render_expr(left, None),
        crate::sql::render_expr(right, None),
        op_str,
    ))
}

pub(crate) fn extract_policy_comparison_for_policy(
    store: &crate::policy_store::PolicyStore,
    index: usize,
    constraint: &str,
    stats: Option<&crate::rewrite_stats::RewriteStatsCell>,
) -> Result<(String, String, &'static str), RewriteError> {
    let expr = store.constraint_expr(index, constraint, stats)?;
    extract_policy_comparison_from_expr(&expr)
}

pub(crate) fn apply_policy_having(
    rewriter: &PassantRewriter,
    select: &mut Select,
    skip_indices: &HashSet<usize>,
) -> Result<(), RewriteError> {
    let table_scope = TableScope::from_select(select);
    let context = RewriteContext::default();
    let (actions, _) = plan_policy_filter_actions(
        rewriter.policy_store(),
        rewriter.catalog(),
        None,
        &table_scope.direct_base_tables,
        &table_scope,
        None,
        &context,
        true,
        skip_indices,
        &HashSet::new(),
    )?;
    apply_policy_resolution_actions(select, &actions, true)
}
