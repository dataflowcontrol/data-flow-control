//! Limit-first policy rewrites: evaluate LIMIT/OFFSET/FETCH before policy enforcement.

use std::collections::{HashMap, HashSet};

use sqlparser::ast::{Expr, Ident, Query, Select, SelectItem, SetExpr, Statement};

use crate::rewrite_strategy::query_has_limit;
use crate::rewriter::{
    PassantRewriter, PolicyResolutionAction, RewriteContext, RewriteError, TableScope,
    apply_policy_resolution_actions, collect_compound_columns_by_name, ensure_projection_aliases,
    expr_contains_aggregate, outer_limited_projection_items, plan_policy_filter_actions,
    projected_column_name, replace_identifiers, select_is_aggregation, unqualify_columns,
};
use crate::sql::{cte, empty_select};
use crate::sql::{
    passant_filter_temp_column, render_expr, render_statement, statement_from_query, table_factor,
    with_ctes,
};

pub(crate) const LIMITED_POLICY_CTE: &str = "__passant_limited";

type OuterPolicyActionsResult = Result<
    (
        Vec<PolicyResolutionAction>,
        Query,
        HashMap<String, (Expr, String)>,
    ),
    RewriteError,
>;

pub(crate) fn wrap_limited_policy_query(
    rewriter: &PassantRewriter,
    query: &Query,
    context: &mut RewriteContext,
) -> Result<Option<Query>, RewriteError> {
    if context.defer_policy_for_outer_limit {
        return Ok(None);
    }
    if !query_has_limit(query) {
        return Ok(None);
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Ok(None);
    };
    if !select_has_applicable_policy(rewriter, select) {
        return Ok(None);
    }
    if select_has_unsupported_limit_policy_shape(select) {
        return Err(RewriteError::unsupported_statement(
            "LIMIT/OFFSET/FETCH policy rewrites do not support SELECT DISTINCT or unexpanded wildcards when hidden filter columns are required",
        ));
    }

    let mut inner = query.clone();
    let mut defer_ctx = context.clone();
    defer_ctx.defer_policy_for_outer_limit = true;
    rewriter.rewrite_query_with_context(&mut inner, &mut defer_ctx)?;

    let inner_select = match inner.body.as_ref() {
        SetExpr::Select(select) => select.as_ref().clone(),
        _ => return Ok(None),
    };

    let Some(wrapped) = build_limited_policy_wrapper(
        rewriter,
        inner,
        &inner_select,
        context,
        LIMITED_POLICY_CTE,
        &defer_ctx.pending_in_semijoin_filters,
    )?
    else {
        return Ok(None);
    };
    Ok(Some(wrapped))
}

pub(crate) fn build_limited_policy_wrapper(
    rewriter: &PassantRewriter,
    inner: Query,
    inner_select: &Select,
    context: &RewriteContext,
    cte_name: &str,
    pending_in_semijoin_filters: &[crate::partial_push::ExtraDfcFilter],
) -> Result<Option<Query>, RewriteError> {
    let table_scope = TableScope::from_select(inner_select);
    let projected_names = projected_select_names(inner_select);
    let is_aggregation = select_is_aggregation(inner_select, &context.aggregate_registry);

    let (actions, _) = plan_policy_filter_actions(
        rewriter.policy_store(),
        rewriter.catalog(),
        context.collect_stats.then_some(&rewriter.stats),
        &mut inner_select.clone(),
        &table_scope.direct_base_tables,
        context.sink.as_deref(),
        context,
        is_aggregation,
        &HashSet::new(),
        &HashSet::new(),
    )?;

    let (outer_actions, mut inner, propagated) = collect_outer_policy_actions(
        actions,
        inner_select,
        inner,
        projected_names,
        &context.aggregate_registry,
    )?;

    let mut outer_actions = outer_actions;
    for extra in pending_in_semijoin_filters {
        let filter = crate::sql::column_comparison(
            &extra.alias,
            extra.op,
            crate::rewriter::parse_expr(&extra.threshold)?,
        )
        .ok_or_else(|| {
            RewriteError::unsupported_statement(
                "limit-first IN semijoin policy rewrite uses unsupported comparison operator",
            )
        })?;
        outer_actions.push(PolicyResolutionAction::Filter {
            filter,
            description: None,
        });
    }

    if outer_actions.is_empty() {
        return Ok(None);
    }

    if let SetExpr::Select(inner_select) = inner.body.as_mut() {
        ensure_projection_aliases(inner_select);
        for (_, (expr, alias)) in propagated {
            inner_select.projection.push(SelectItem::ExprWithAlias {
                expr,
                alias: Ident::new(alias),
            });
        }
    }

    let outer_projection = if let SetExpr::Select(inner_select) = inner.body.as_ref() {
        outer_limited_projection_items(inner_select)
    } else {
        return Ok(None);
    };

    let mut outer_select = empty_select();
    outer_select.projection = outer_projection;
    outer_select.from = vec![sqlparser::ast::TableWithJoins {
        relation: table_factor(cte_name),
        joins: Vec::new(),
    }];
    apply_policy_resolution_actions(
        &mut outer_select,
        &outer_actions,
        false,
        context,
        rewriter.policy_store(),
        rewriter.catalog(),
    )?;

    Ok(Some(with_ctes(
        vec![cte(cte_name, inner)],
        SetExpr::Select(Box::new(outer_select)),
    )))
}

/// Returns `Ok(None)` when the query shape is eligible but no policy actions apply.
pub(crate) fn try_render_limited_policy_wrapper(
    rewriter: &PassantRewriter,
    statement: &Statement,
    cte_name: &str,
) -> Result<Option<String>, RewriteError> {
    let Statement::Query(query) = statement else {
        return Ok(None);
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Ok(None);
    };
    if !query_has_limit(query) {
        return Ok(None);
    }
    if !select_has_applicable_policy(rewriter, select) {
        return Ok(None);
    }
    if select_has_unsupported_limit_policy_shape(select) {
        return Err(RewriteError::unsupported_statement(
            "LIMIT/OFFSET/FETCH policy rewrites do not support SELECT DISTINCT or unexpanded wildcards when hidden filter columns are required",
        ));
    }
    let context = RewriteContext::default();
    let Some(wrapped) =
        build_limited_policy_wrapper(rewriter, (**query).clone(), select, &context, cte_name, &[])?
    else {
        return Ok(None);
    };
    Ok(Some(render_statement(&statement_from_query(wrapped), None)))
}

/// Build a limit-first policy wrapper and render SQL (shared by partial-push limit scan).
pub(crate) fn render_limited_policy_wrapper(
    rewriter: &PassantRewriter,
    statement: &Statement,
    cte_name: &str,
) -> Result<String, RewriteError> {
    try_render_limited_policy_wrapper(rewriter, statement, cte_name)?.ok_or_else(|| {
        RewriteError::unsupported_statement(
            "limit-first policy rewrite found no applicable policy actions",
        )
    })
}

fn select_has_applicable_policy(rewriter: &PassantRewriter, select: &Select) -> bool {
    let tables = TableScope::from_select(select).direct_base_tables;
    rewriter
        .policy_store()
        .candidate_scope_lookup(&tables, None, crate::MultiSourceLookupMode::AnyOverlap)
        .iter()
        .any(|index| rewriter.policy_at(index).is_some())
}

fn select_has_unsupported_limit_policy_shape(select: &Select) -> bool {
    if matches!(select.distinct, Some(sqlparser::ast::Distinct::Distinct)) {
        return true;
    }
    select.projection.iter().any(|item| {
        matches!(
            item,
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _)
        )
    })
}

fn collect_outer_policy_actions(
    actions: Vec<PolicyResolutionAction>,
    inner_select: &Select,
    inner: Query,
    projected_names: HashSet<String>,
    registry: &crate::aggregate_registry::AggregateRegistry,
) -> OuterPolicyActionsResult {
    let is_aggregation = select_is_aggregation(inner_select, registry);
    let mut outer_actions = Vec::new();
    let mut propagated_filter_columns = HashMap::new();

    for action in actions {
        let rewritten = rewrite_action_filter_for_outer(
            action,
            &projected_names,
            is_aggregation,
            registry,
            &mut propagated_filter_columns,
        );
        outer_actions.push(rewritten);
    }

    Ok((outer_actions, inner, propagated_filter_columns))
}

fn rewrite_action_filter_for_outer(
    action: PolicyResolutionAction,
    projected_names: &HashSet<String>,
    is_aggregation: bool,
    registry: &crate::aggregate_registry::AggregateRegistry,
    propagated: &mut HashMap<String, (Expr, String)>,
) -> PolicyResolutionAction {
    match action {
        PolicyResolutionAction::Filter {
            filter,
            description,
        } => PolicyResolutionAction::Filter {
            filter: rewrite_filter_for_outer(
                filter,
                projected_names,
                is_aggregation,
                registry,
                propagated,
            ),
            description,
        },
        PolicyResolutionAction::TupleUdf {
            filter,
            udf_name,
            description,
        } => PolicyResolutionAction::TupleUdf {
            filter: rewrite_filter_for_outer(
                filter,
                projected_names,
                is_aggregation,
                registry,
                propagated,
            ),
            udf_name,
            description,
        },
        PolicyResolutionAction::RelationUdf {
            filter,
            udf_name,
            description,
        } => PolicyResolutionAction::RelationUdf {
            filter: rewrite_filter_for_outer(
                filter,
                projected_names,
                is_aggregation,
                registry,
                propagated,
            ),
            udf_name,
            description,
        },
        PolicyResolutionAction::Ui {
            filter,
            spec,
            policy,
        } => PolicyResolutionAction::Ui {
            filter: rewrite_filter_for_outer(
                filter,
                projected_names,
                is_aggregation,
                registry,
                propagated,
            ),
            spec,
            policy,
        },
    }
}

fn rewrite_filter_for_outer(
    mut expr: Expr,
    projected_names: &HashSet<String>,
    is_aggregation: bool,
    registry: &crate::aggregate_registry::AggregateRegistry,
    propagated: &mut HashMap<String, (Expr, String)>,
) -> Expr {
    if is_aggregation && expr_contains_aggregate(&expr, registry) {
        let mut agg_exprs = Vec::new();
        collect_aggregate_exprs(&expr, registry, &mut agg_exprs);
        for agg_expr in agg_exprs {
            let key = format!("agg::{}", render_expr(&agg_expr, None));
            let alias = if let Some((_, alias)) = propagated.get(&key) {
                alias.clone()
            } else {
                let alias = passant_filter_temp_column(&format!("agg_{}", propagated.len()));
                propagated.insert(key.clone(), (agg_expr.clone(), alias.clone()));
                alias
            };
            replace_expr_exact(&mut expr, &agg_expr, &Expr::Identifier(Ident::new(alias)));
        }
    }

    let mut source_columns = HashMap::new();
    collect_compound_columns_by_name(&expr, &mut source_columns);
    unqualify_columns(&mut expr);
    for (name, source_expr) in source_columns {
        if !projected_names.contains(&name.to_ascii_lowercase()) {
            let alias = passant_filter_temp_column(&name);
            propagated.entry(name).or_insert((source_expr, alias));
        }
    }
    let replacements = propagated
        .iter()
        .filter(|(name, _)| !name.starts_with("agg::"))
        .map(|(name, (_, alias))| (name.clone(), alias.clone()))
        .collect::<HashMap<_, _>>();
    replace_identifiers(&mut expr, &replacements);
    expr
}

fn collect_aggregate_exprs(
    expr: &Expr,
    registry: &crate::aggregate_registry::AggregateRegistry,
    out: &mut Vec<Expr>,
) {
    match expr {
        Expr::Function(function) => {
            if registry.is_aggregate_call(function) {
                out.push(expr.clone());
            }
            if let sqlparser::ast::FunctionArguments::List(args) = &function.args {
                for arg in &args.args {
                    match arg {
                        sqlparser::ast::FunctionArg::Unnamed(
                            sqlparser::ast::FunctionArgExpr::Expr(inner),
                        )
                        | sqlparser::ast::FunctionArg::Named {
                            arg: sqlparser::ast::FunctionArgExpr::Expr(inner),
                            ..
                        }
                        | sqlparser::ast::FunctionArg::ExprNamed {
                            arg: sqlparser::ast::FunctionArgExpr::Expr(inner),
                            ..
                        } => {
                            collect_aggregate_exprs(inner, registry, out);
                        }
                        _ => {}
                    }
                }
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_aggregate_exprs(left, registry, out);
            collect_aggregate_exprs(right, registry, out);
        }
        Expr::Nested(inner)
        | Expr::UnaryOp { expr: inner, .. }
        | Expr::IsFalse(inner)
        | Expr::IsNotFalse(inner)
        | Expr::IsTrue(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner) => collect_aggregate_exprs(inner, registry, out),
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_aggregate_exprs(expr, registry, out);
            collect_aggregate_exprs(low, registry, out);
            collect_aggregate_exprs(high, registry, out);
        }
        Expr::InList { expr, list, .. } => {
            collect_aggregate_exprs(expr, registry, out);
            for item in list {
                collect_aggregate_exprs(item, registry, out);
            }
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                collect_aggregate_exprs(operand, registry, out);
            }
            for cond in conditions {
                collect_aggregate_exprs(cond, registry, out);
            }
            for result in results {
                collect_aggregate_exprs(result, registry, out);
            }
            if let Some(else_result) = else_result {
                collect_aggregate_exprs(else_result, registry, out);
            }
        }
        _ => {}
    }
}

fn replace_expr_exact(expr: &mut Expr, needle: &Expr, replacement: &Expr) {
    if expr == needle {
        *expr = replacement.clone();
        return;
    }
    match expr {
        Expr::BinaryOp { left, right, .. } => {
            replace_expr_exact(left, needle, replacement);
            replace_expr_exact(right, needle, replacement);
        }
        Expr::Nested(inner)
        | Expr::UnaryOp { expr: inner, .. }
        | Expr::IsFalse(inner)
        | Expr::IsNotFalse(inner)
        | Expr::IsTrue(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner) => replace_expr_exact(inner, needle, replacement),
        Expr::Between {
            expr, low, high, ..
        } => {
            replace_expr_exact(expr, needle, replacement);
            replace_expr_exact(low, needle, replacement);
            replace_expr_exact(high, needle, replacement);
        }
        Expr::InList { expr, list, .. } => {
            replace_expr_exact(expr, needle, replacement);
            for item in list {
                replace_expr_exact(item, needle, replacement);
            }
        }
        Expr::Function(function) => {
            if let sqlparser::ast::FunctionArguments::List(args) = &mut function.args {
                for arg in &mut args.args {
                    match arg {
                        sqlparser::ast::FunctionArg::Unnamed(
                            sqlparser::ast::FunctionArgExpr::Expr(inner),
                        )
                        | sqlparser::ast::FunctionArg::Named {
                            arg: sqlparser::ast::FunctionArgExpr::Expr(inner),
                            ..
                        }
                        | sqlparser::ast::FunctionArg::ExprNamed {
                            arg: sqlparser::ast::FunctionArgExpr::Expr(inner),
                            ..
                        } => {
                            replace_expr_exact(inner, needle, replacement);
                        }
                        _ => {}
                    }
                }
            }
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                replace_expr_exact(operand, needle, replacement);
            }
            for cond in conditions {
                replace_expr_exact(cond, needle, replacement);
            }
            for result in results {
                replace_expr_exact(result, needle, replacement);
            }
            if let Some(else_result) = else_result {
                replace_expr_exact(else_result, needle, replacement);
            }
        }
        _ => {}
    }
}

pub(crate) fn projected_select_names(select: &Select) -> HashSet<String> {
    select
        .projection
        .iter()
        .filter_map(|item| match item {
            SelectItem::UnnamedExpr(expr) => projected_column_name(expr),
            SelectItem::ExprWithAlias { alias, .. } => Some(alias.value.clone()),
            _ => None,
        })
        .map(|name| name.to_ascii_lowercase())
        .collect()
}
