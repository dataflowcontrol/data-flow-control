//! Defer INSERT policies that combine non-distributive source aggregates with
//! projected output columns until the aggregation CTE and outer SELECT body.

use std::collections::{HashMap, HashSet};

use sqlparser::ast::{
    BinaryOperator, DuplicateTreatment, Expr, Function, FunctionArguments, Ident, Query, Select,
    SelectItem, SetExpr, TableFactor, Value,
};

use crate::aggregate_registry::AggregateRegistry;
use crate::catalog::TableCatalog;
use crate::identifiers::TableKey;
use crate::policy::{PolicyIr, Resolution};
use crate::policy_store::PolicyStore;
use crate::sql::{passant_internal_name, render_expr};

use super::RewriteError;
use super::columns::{apply_output_marker_replacements, apply_policy_sink_column_replacements};
use super::expr::{first_function_expr, projection_expr_and_name};
use super::helpers::{OUTPUT_MARKER, OutputColumnMapping, flatten_and};
use super::policy_expr::{ConstraintExprCtx, non_distributive_aggregates};
use super::projection::select_is_aggregation;
use super::scope::TableScope;
use super::types::{
    InsertOutputPolicyPlan, InsertRewriteSite, PolicyApplicability, RewriteContext,
};

#[derive(Debug, Clone)]
struct AggregateProjection {
    expr: Expr,
    alias: String,
}

pub(crate) fn plan_insert_output_policy(
    store: &PolicyStore,
    query: &Query,
    output_mapping: &OutputColumnMapping,
    registry: &AggregateRegistry,
    sink: Option<&str>,
) -> Result<Option<InsertOutputPolicyPlan>, RewriteError> {
    let Some(with) = query.with.as_ref() else {
        return Ok(None);
    };
    let SetExpr::Select(body) = query.body.as_ref() else {
        return Ok(None);
    };
    if select_is_aggregation(body, registry) {
        return Ok(None);
    }

    let mut policy_indices = HashSet::new();
    let mut aggregate_aliases = HashMap::new();
    let mut render_select_body_only = false;
    let mut aggregation_cte = None;
    let mut policy_source_tables = None;

    for index in store.policy_indices() {
        let Some(policy) = store.policy(index) else {
            continue;
        };
        let PolicyIr::Pgn {
            sources,
            constraint,
            on_fail,
            sink: policy_sink,
            ..
        } = policy;
        if !matches!(
            on_fail,
            Resolution::Kill | Resolution::Udf(_) | Resolution::RelationUdf(_)
        ) {
            continue;
        }
        if !sources.is_empty()
            && !policy_needs_insert_output_scope(
                store,
                index,
                policy,
                constraint,
                output_mapping,
                registry,
            )?
        {
            continue;
        }
        if sources.is_empty() {
            continue;
        }
        let Some(cte_name) = find_aggregation_cte(with, body, sources, registry) else {
            continue;
        };
        if !has_upstream_source_scan(with, &cte_name, sources, registry) {
            continue;
        }
        let projections = collect_aggregate_projections(store, index, constraint, registry)?;
        if projections.is_empty() {
            continue;
        }
        for projection in &projections {
            aggregate_aliases.insert(
                render_expr(&projection.expr, None),
                projection.alias.clone(),
            );
        }
        policy_indices.insert(index);
        aggregation_cte = Some(cte_name.clone());
        policy_source_tables = Some(cte_transitive_source_tables(with, &cte_name));
        if policy_sink.is_none() && constraint.contains(OUTPUT_MARKER) {
            render_select_body_only = true;
        }
        if policy_sink.is_some()
            && sink.is_some_and(|insert_sink| policy_sink.as_deref() == Some(insert_sink))
        {
            render_select_body_only = false;
        }
    }

    let Some(aggregation_cte) = aggregation_cte else {
        return Ok(None);
    };
    let policy_source_tables =
        policy_source_tables.expect("aggregation cte planning should record source tables");
    Ok(Some(InsertOutputPolicyPlan {
        policy_indices,
        aggregation_cte,
        policy_source_tables,
        aggregate_aliases,
        render_select_body_only,
    }))
}

fn policy_needs_insert_output_scope(
    store: &PolicyStore,
    index: usize,
    policy: &PolicyIr,
    constraint: &str,
    output_mapping: &OutputColumnMapping,
    registry: &AggregateRegistry,
) -> Result<bool, RewriteError> {
    let constraint_ctx = ConstraintExprCtx {
        store,
        index,
        stats: None,
    };
    let expr = constraint_ctx.expr(constraint)?;
    if non_distributive_aggregates(&expr, registry)?.is_empty() {
        return Ok(false);
    }
    Ok(constraint_references_output_columns(
        &expr,
        policy,
        output_mapping,
    ))
}

fn constraint_references_output_columns(
    expr: &Expr,
    policy: &PolicyIr,
    output_mapping: &OutputColumnMapping,
) -> bool {
    if expr_contains_output_marker(expr) {
        return true;
    }
    let PolicyIr::Pgn { sink, .. } = policy;
    if sink.is_none() {
        return false;
    }
    let Some(sink_name) = sink.as_deref() else {
        return false;
    };
    output_mapping
        .expr_by_column
        .keys()
        .any(|column| constraint_text_references_column(expr, sink_name, column))
}

fn expr_contains_output_marker(expr: &Expr) -> bool {
    match expr {
        Expr::Identifier(ident) if ident.value.eq_ignore_ascii_case(OUTPUT_MARKER) => true,
        Expr::CompoundIdentifier(parts)
            if { parts.len() >= 2 && parts[0].value.eq_ignore_ascii_case(OUTPUT_MARKER) } =>
        {
            true
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_contains_output_marker(left) || expr_contains_output_marker(right)
        }
        Expr::Nested(inner) => expr_contains_output_marker(inner),
        Expr::UnaryOp { expr, .. } => expr_contains_output_marker(expr),
        Expr::Function(function) => function_references_output(function),
        Expr::Between {
            expr, low, high, ..
        } => {
            expr_contains_output_marker(expr)
                || expr_contains_output_marker(low)
                || expr_contains_output_marker(high)
        }
        Expr::InList { expr, list, .. } => {
            expr_contains_output_marker(expr) || list.iter().any(expr_contains_output_marker)
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            operand
                .as_ref()
                .is_some_and(|expr| expr_contains_output_marker(expr))
                || conditions.iter().any(expr_contains_output_marker)
                || results.iter().any(expr_contains_output_marker)
                || else_result
                    .as_ref()
                    .is_some_and(|expr| expr_contains_output_marker(expr))
        }
        _ => false,
    }
}

fn function_references_output(function: &Function) -> bool {
    let FunctionArguments::List(args) = &function.args else {
        return false;
    };
    args.args.iter().any(|arg| match arg {
        sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr))
        | sqlparser::ast::FunctionArg::Named {
            arg: sqlparser::ast::FunctionArgExpr::Expr(expr),
            ..
        }
        | sqlparser::ast::FunctionArg::ExprNamed {
            arg: sqlparser::ast::FunctionArgExpr::Expr(expr),
            ..
        } => expr_contains_output_marker(expr),
        _ => false,
    })
}

fn constraint_text_references_column(expr: &Expr, sink: &str, column: &str) -> bool {
    let rendered = render_expr(expr, None).to_ascii_lowercase();
    rendered.contains(&format!(
        "{}.{}",
        sink.to_ascii_lowercase(),
        column.to_ascii_lowercase()
    )) || rendered.contains(&format!(
        "\"{}\".\"{}\"",
        sink.to_ascii_lowercase(),
        column.to_ascii_lowercase()
    ))
}

fn find_aggregation_cte(
    with: &sqlparser::ast::With,
    body: &Select,
    policy_sources: &[String],
    registry: &AggregateRegistry,
) -> Option<String> {
    let body_tables = TableScope::from_select(body).direct_base_tables;
    for cte in &with.cte_tables {
        let name = cte.alias.name.value.clone();
        if !body_tables.contains(&TableKey::new(&name)) {
            continue;
        }
        let SetExpr::Select(select) = cte.query.body.as_ref() else {
            continue;
        };
        if !select_is_aggregation(select, registry) {
            continue;
        }
        let source_tables = cte_transitive_source_tables(with, &name);
        if policy_sources
            .iter()
            .all(|source| source_tables.contains(&TableKey::new(source)))
        {
            return Some(name);
        }
    }
    None
}

fn cte_transitive_source_tables(with: &sqlparser::ast::With, cte_name: &str) -> HashSet<TableKey> {
    let mut visiting = HashSet::new();
    let mut sources = HashSet::new();
    collect_cte_transitive_source_tables(with, cte_name, &mut visiting, &mut sources);
    sources
}

fn collect_cte_transitive_source_tables(
    with: &sqlparser::ast::With,
    cte_name: &str,
    visiting: &mut HashSet<String>,
    sources: &mut HashSet<TableKey>,
) {
    if !visiting.insert(cte_name.to_string()) {
        return;
    }
    let Some(select) = cte_select(with, cte_name) else {
        sources.insert(TableKey::new(cte_name));
        return;
    };
    let scope = TableScope::from_select(select);
    for key in scope.direct_base_tables {
        let name = key.as_str();
        if cte_select(with, name).is_some() {
            collect_cte_transitive_source_tables(with, name, visiting, sources);
        } else {
            sources.insert(key);
        }
    }
    for table in &select.from {
        collect_factor_transitive_source_tables(with, &table.relation, visiting, sources);
        for join in &table.joins {
            collect_factor_transitive_source_tables(with, &join.relation, visiting, sources);
        }
    }
}

fn collect_factor_transitive_source_tables(
    with: &sqlparser::ast::With,
    factor: &TableFactor,
    visiting: &mut HashSet<String>,
    sources: &mut HashSet<TableKey>,
) {
    match factor {
        TableFactor::Table { name, .. } => {
            let base = crate::identifiers::TableName::from_object_name(name);
            let key = TableKey::from_table(&base);
            let cte_name = key.as_str();
            if cte_select(with, cte_name).is_some() {
                collect_cte_transitive_source_tables(with, cte_name, visiting, sources);
            } else {
                sources.insert(key);
            }
        }
        TableFactor::Derived { subquery, .. } => {
            if let SetExpr::Select(select) = subquery.body.as_ref() {
                for table in &select.from {
                    collect_factor_transitive_source_tables(
                        with,
                        &table.relation,
                        visiting,
                        sources,
                    );
                    for join in &table.joins {
                        collect_factor_transitive_source_tables(
                            with,
                            &join.relation,
                            visiting,
                            sources,
                        );
                    }
                }
            }
        }
        _ => {}
    }
}

fn has_upstream_source_scan(
    with: &sqlparser::ast::With,
    aggregation_cte: &str,
    policy_sources: &[String],
    registry: &AggregateRegistry,
) -> bool {
    let mut visiting = HashSet::new();
    has_upstream_source_scan_from(
        with,
        aggregation_cte,
        policy_sources,
        registry,
        &mut visiting,
    )
}

fn has_upstream_source_scan_from(
    with: &sqlparser::ast::With,
    cte_name: &str,
    policy_sources: &[String],
    registry: &AggregateRegistry,
    visiting: &mut HashSet<String>,
) -> bool {
    if !visiting.insert(cte_name.to_string()) {
        return false;
    }
    let Some(select) = cte_select(with, cte_name) else {
        return false;
    };
    let scope = TableScope::from_select(select);
    if !select_is_aggregation(select, registry)
        && policy_sources
            .iter()
            .any(|source| scope.direct_base_tables.contains(&TableKey::new(source)))
    {
        return true;
    }
    for dep in cte_dependencies(select) {
        if has_upstream_source_scan_from(with, &dep, policy_sources, registry, visiting) {
            return true;
        }
    }
    false
}

fn cte_select<'a>(with: &'a sqlparser::ast::With, cte_name: &str) -> Option<&'a Select> {
    let cte = with
        .cte_tables
        .iter()
        .find(|cte| cte.alias.name.value.eq_ignore_ascii_case(cte_name))?;
    match cte.query.body.as_ref() {
        SetExpr::Select(select) => Some(select.as_ref()),
        _ => None,
    }
}

fn cte_dependencies(select: &Select) -> Vec<String> {
    TableScope::from_select(select)
        .direct_base_tables
        .into_iter()
        .map(|key| key.as_str().to_string())
        .collect()
}

fn collect_aggregate_projections(
    store: &PolicyStore,
    index: usize,
    constraint: &str,
    registry: &AggregateRegistry,
) -> Result<Vec<AggregateProjection>, RewriteError> {
    let constraint_ctx = ConstraintExprCtx {
        store,
        index,
        stats: None,
    };
    let expr = constraint_ctx.expr(constraint)?;
    let mut projections = Vec::new();
    let mut seen = HashSet::new();
    for conjunct in flatten_and(&expr) {
        if let Some(projection) = count_distinct_one_projection(&conjunct) {
            let key = render_expr(&projection.expr, None);
            if seen.insert(key) {
                projections.push(projection);
            }
            continue;
        }
        collect_arbitrary_projections(&conjunct, registry, &mut projections, &mut seen);
    }
    Ok(projections)
}

fn count_distinct_one_projection(expr: &Expr) -> Option<AggregateProjection> {
    let Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    } = expr
    else {
        return None;
    };
    let Expr::Function(function) = left.as_ref() else {
        return None;
    };
    if !function.name.to_string().eq_ignore_ascii_case("count") {
        return None;
    }
    let FunctionArguments::List(args) = &function.args else {
        return None;
    };
    if args.duplicate_treatment != Some(DuplicateTreatment::Distinct) {
        return None;
    }
    if !matches!(right.as_ref(), Expr::Value(Value::Number(value, _)) if value == "1") {
        return None;
    }
    Some(AggregateProjection {
        expr: expr.clone(),
        alias: passant_internal_name("__passant", "count_distinct_one"),
    })
}

fn collect_arbitrary_projections(
    expr: &Expr,
    registry: &AggregateRegistry,
    projections: &mut Vec<AggregateProjection>,
    seen: &mut HashSet<String>,
) {
    match expr {
        Expr::Function(function)
            if registry.is_aggregate_call(function)
                && function.name.to_string().eq_ignore_ascii_case("arbitrary") =>
        {
            let key = render_expr(&Expr::Function(function.clone()), None);
            if seen.insert(key) {
                let suffix = first_function_expr(function)
                    .and_then(|arg| match arg {
                        Expr::CompoundIdentifier(parts) if !parts.is_empty() => {
                            Some(parts.last()?.value.clone())
                        }
                        Expr::Identifier(ident) => Some(ident.value.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "value".to_string());
                projections.push(AggregateProjection {
                    expr: Expr::Function(function.clone()),
                    alias: passant_internal_name("__passant", &format!("arbitrary_{suffix}")),
                });
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_arbitrary_projections(left, registry, projections, seen);
            collect_arbitrary_projections(right, registry, projections, seen);
        }
        Expr::Nested(inner) => collect_arbitrary_projections(inner, registry, projections, seen),
        Expr::UnaryOp { expr, .. } => {
            collect_arbitrary_projections(expr, registry, projections, seen)
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                collect_arbitrary_projections(operand, registry, projections, seen);
            }
            for condition in conditions {
                collect_arbitrary_projections(condition, registry, projections, seen);
            }
            for result in results {
                collect_arbitrary_projections(result, registry, projections, seen);
            }
            if let Some(else_result) = else_result {
                collect_arbitrary_projections(else_result, registry, projections, seen);
            }
        }
        _ => {}
    }
}

pub(crate) fn should_defer_insert_output_policy(
    plan: &InsertOutputPolicyPlan,
    site: &InsertRewriteSite,
    policy_index: usize,
) -> bool {
    if !plan.policy_indices.contains(&policy_index) {
        return false;
    }
    match site {
        InsertRewriteSite::Cte(name) if name == &plan.aggregation_cte => true,
        InsertRewriteSite::Cte(_) => true,
        InsertRewriteSite::Body => false,
    }
}

pub(crate) fn inject_insert_output_aggregate_columns(
    select: &mut Select,
    plan: &InsertOutputPolicyPlan,
    store: &PolicyStore,
    registry: &AggregateRegistry,
) -> Result<(), RewriteError> {
    for index in &plan.policy_indices {
        let Some(policy) = store.policy(*index) else {
            continue;
        };
        let PolicyIr::Pgn { constraint, .. } = policy;
        for projection in collect_aggregate_projections(store, *index, constraint, registry)? {
            if projection_already_present(select, &projection.expr, &projection.alias) {
                continue;
            }
            select.projection.push(SelectItem::ExprWithAlias {
                expr: projection.expr,
                alias: Ident::new(&projection.alias),
            });
        }
    }
    Ok(())
}

fn projection_already_present(select: &Select, expr: &Expr, alias: &str) -> bool {
    let key = crate::sql::ExprKey::from_expr(expr);
    select.projection.iter().any(|item| {
        if let Some((proj_expr, proj_alias)) = projection_expr_and_name(item) {
            if proj_alias.is_some_and(|name| name == alias) {
                return true;
            }
            return crate::sql::ExprKey::from_expr(proj_expr) == key;
        }
        false
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_insert_body_policy_filter(
    store: &PolicyStore,
    _catalog: &TableCatalog,
    policy_index: usize,
    policy: &PolicyIr,
    applicability: PolicyApplicability,
    context: &RewriteContext,
    table_scope: &TableScope,
    plan: &InsertOutputPolicyPlan,
) -> Result<Expr, RewriteError> {
    let PolicyIr::Pgn {
        sources,
        constraint,
        sink_alias,
        source_aliases,
        ..
    } = policy;
    if applicability == PolicyApplicability::RequiredSourceMissing {
        return Ok(super::expr::bool_literal(false));
    }
    let constraint_ctx = ConstraintExprCtx {
        store,
        index: policy_index,
        stats: None,
    };
    let mut expr = constraint_ctx.expr(constraint)?;
    expr = super::columns::replace_source_alias_qualifiers(expr, source_aliases);
    if let Some(sink) = &context.sink {
        expr = apply_policy_sink_column_replacements(
            expr,
            sink,
            sink_alias,
            sources,
            &context.sink_expr_by_column,
            &context.ambiguous_output_columns,
        )?;
    } else if !context.sink_expr_by_column.is_empty()
        || !context.ambiguous_output_columns.is_empty()
    {
        expr = apply_output_marker_replacements(
            expr,
            &context.sink_expr_by_column,
            &context.ambiguous_output_columns,
        )?;
    }
    expr = replace_aggregate_expressions_with_aliases(expr, plan)?;
    super::columns::rewrite_column_qualifiers(&mut expr, &table_scope.alias_by_base);
    Ok(expr)
}

fn replace_aggregate_expressions_with_aliases(
    expr: Expr,
    plan: &InsertOutputPolicyPlan,
) -> Result<Expr, RewriteError> {
    Ok(replace_aggregate_expr_recursive(expr, plan))
}

fn replace_aggregate_expr_recursive(expr: Expr, plan: &InsertOutputPolicyPlan) -> Expr {
    if let Some(alias) = plan.aggregate_aliases.get(&render_expr(&expr, None)) {
        return Expr::Identifier(Ident::new(alias));
    }
    if let Some(projection) = count_distinct_one_projection(&expr) {
        return Expr::Identifier(Ident::new(&projection.alias));
    }
    match expr {
        Expr::Function(function) => {
            let key = render_expr(&Expr::Function(function.clone()), None);
            if let Some(alias) = plan.aggregate_aliases.get(&key) {
                return Expr::Identifier(Ident::new(alias));
            }
            Expr::Function(function)
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(replace_aggregate_expr_recursive(*left, plan)),
            op,
            right: Box::new(replace_aggregate_expr_recursive(*right, plan)),
        },
        Expr::Nested(inner) => {
            Expr::Nested(Box::new(replace_aggregate_expr_recursive(*inner, plan)))
        }
        Expr::UnaryOp { op, expr } => Expr::UnaryOp {
            op,
            expr: Box::new(replace_aggregate_expr_recursive(*expr, plan)),
        },
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => Expr::Between {
            expr: Box::new(replace_aggregate_expr_recursive(*expr, plan)),
            negated,
            low: Box::new(replace_aggregate_expr_recursive(*low, plan)),
            high: Box::new(replace_aggregate_expr_recursive(*high, plan)),
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(replace_aggregate_expr_recursive(*expr, plan)),
            list: list
                .into_iter()
                .map(|item| replace_aggregate_expr_recursive(item, plan))
                .collect(),
            negated,
        },
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => Expr::Case {
            operand: operand.map(|expr| Box::new(replace_aggregate_expr_recursive(*expr, plan))),
            conditions: conditions
                .into_iter()
                .map(|cond| replace_aggregate_expr_recursive(cond, plan))
                .collect(),
            results: results
                .into_iter()
                .map(|result| replace_aggregate_expr_recursive(result, plan))
                .collect(),
            else_result: else_result
                .map(|expr| Box::new(replace_aggregate_expr_recursive(*expr, plan))),
        },
        other => other,
    }
}
