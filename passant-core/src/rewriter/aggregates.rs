use std::collections::HashMap;

use sqlparser::ast::{
    Assignment, AssignmentTarget, BinaryOperator, DuplicateTreatment, Expr, FunctionArg,
    FunctionArgExpr, FunctionArguments, Select, SelectItem, Statement, TableWithJoins,
};

use crate::diagnostics::RewriteError;
use crate::sql::{
    alias_expr, and_exprs, binary_comparison, bool_literal, case_when, duckdb_array, function_call,
    grouped_select, int_literal, is_not_null, null_literal, object_name, passant_agg_temp_column,
    query_from_select, rename_table_refs, replace_expr_subtrees, statement_from_query, table_alias,
    table_factor,
};

use super::columns::replace_sink_columns;
use super::expr::{
    first_function_expr, is_aggregate_name, parse_expr, parse_expr_or_identity,
    projected_column_name, projection_expr_and_name,
};
use super::types::{RewriteContext, SourceAggregate};

pub(crate) fn aggregate_temp_column(index: usize) -> String {
    passant_agg_temp_column(index)
}

pub(crate) fn transform_scan_aggregates(expr: Expr) -> Result<Expr, RewriteError> {
    if let Some(rewritten) = rewrite_count_distinct_equality(&expr)? {
        return Ok(rewritten);
    }
    Ok(transform_scan_aggregates_recursive(expr))
}

fn rewrite_count_distinct_equality(expr: &Expr) -> Result<Option<Expr>, RewriteError> {
    let Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    } = expr
    else {
        return Ok(None);
    };
    let Some(col) = count_distinct_inner_column(left) else {
        return Ok(None);
    };
    let Expr::Value(value) = &**right else {
        return Ok(None);
    };
    if value.to_string() != "1" {
        return Ok(None);
    }
    Ok(Some(is_not_null(col)))
}

fn count_distinct_inner_column(expr: &Expr) -> Option<Expr> {
    let Expr::Function(function) = expr else {
        return None;
    };
    if !function.name.to_string().eq_ignore_ascii_case("count") || !function_is_distinct(function) {
        return None;
    }
    first_function_expr(function)
}

fn transform_scan_aggregates_recursive(expr: Expr) -> Expr {
    match expr {
        Expr::Function(function) => transform_scan_aggregate_function(function),
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(transform_scan_aggregates_recursive(*left)),
            op,
            right: Box::new(transform_scan_aggregates_recursive(*right)),
        },
        Expr::Nested(inner) => Expr::Nested(Box::new(transform_scan_aggregates_recursive(*inner))),
        Expr::UnaryOp { op, expr } => Expr::UnaryOp {
            op,
            expr: Box::new(transform_scan_aggregates_recursive(*expr)),
        },
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => Expr::Case {
            operand: operand.map(|expr| Box::new(transform_scan_aggregates_recursive(*expr))),
            conditions: conditions
                .into_iter()
                .map(transform_scan_aggregates_recursive)
                .collect(),
            results: results
                .into_iter()
                .map(transform_scan_aggregates_recursive)
                .collect(),
            else_result: else_result
                .map(|expr| Box::new(transform_scan_aggregates_recursive(*expr))),
        },
        other => other,
    }
}

fn transform_scan_aggregate_function(function: sqlparser::ast::Function) -> Expr {
    let name = function.name.to_string();
    let lower = name.to_ascii_lowercase();
    if matches!(lower.as_str(), "count_if" | "countif") {
        if let Some(condition) = first_function_expr(&function) {
            return case_when(condition, int_literal(1), int_literal(0));
        }
        return int_literal(0);
    }
    if lower == "array_agg" {
        if let Some(column) = first_function_expr(&function) {
            return duckdb_array(column);
        }
        return duckdb_array(null_literal());
    }
    if lower == "count"
        && function_is_distinct(&function)
        && let Some(column) = first_function_expr(&function)
    {
        return column;
    }
    if is_count_like_aggregate(&lower, &function) {
        return parse_expr_or_identity("1");
    }
    if is_aggregate_name(&name) {
        return first_function_expr(&function).unwrap_or(Expr::Function(function));
    }
    Expr::Function(function)
}

fn is_count_like_aggregate(name: &str, function: &sqlparser::ast::Function) -> bool {
    matches!(
        name,
        "count" | "count_star" | "approx_count_distinct" | "approx_distinct" | "regr_count"
    ) || (name == "count" && function_is_distinct(function))
}

fn function_is_distinct(function: &sqlparser::ast::Function) -> bool {
    match &function.args {
        FunctionArguments::List(list) => {
            list.duplicate_treatment == Some(DuplicateTreatment::Distinct)
        }
        _ => false,
    }
}

pub(crate) fn is_scan_transformable_non_distributive(aggregate: &str) -> bool {
    let lower = aggregate.to_ascii_lowercase();
    lower.contains("array_agg") || lower.contains("count_if") || lower.contains("countif")
}

fn filter_aggregate_to_case_expr(
    aggregate: &Expr,
    select: &Select,
    context: &RewriteContext,
    sink: &str,
) -> Result<Option<Expr>, RewriteError> {
    let Expr::Function(function) = aggregate else {
        return Ok(None);
    };
    let Some(filter) = function.filter.as_ref() else {
        return Ok(None);
    };
    let Some(agg_arg) = first_function_expr(function) else {
        return Ok(None);
    };

    let mut output_map = HashMap::new();
    for item in &select.projection {
        let Some((expr, alias)) = projection_expr_and_name(item) else {
            continue;
        };
        if let Some(alias) = alias {
            output_map.insert(alias.to_ascii_lowercase(), expr.clone());
        }
        if let Some(name) = projected_column_name(expr) {
            output_map.insert(name.to_ascii_lowercase(), expr.clone());
        }
    }
    for (column, expr) in &context.sink_expr_by_column {
        output_map.insert(column.to_ascii_lowercase(), expr.clone());
    }

    let mut condition = filter.as_ref().clone();
    condition = replace_sink_columns(condition, sink, &context.sink_expr_by_column);
    condition = replace_sink_columns(condition, "_OUTPUT_", &context.sink_expr_by_column);

    if filter_references_select_outputs(&condition, &output_map) {
        condition = replace_output_column_refs(condition, &output_map);
    }

    let mut agg_value = agg_arg.clone();
    agg_value = replace_sink_columns(agg_value, sink, &context.sink_expr_by_column);
    agg_value = replace_sink_columns(agg_value, "_OUTPUT_", &context.sink_expr_by_column);

    Ok(Some(case_when(condition, agg_value, int_literal(0))))
}

fn filter_references_select_outputs(condition: &Expr, output_map: &HashMap<String, Expr>) -> bool {
    match condition {
        Expr::Identifier(ident) => output_map.contains_key(&ident.value.to_ascii_lowercase()),
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            output_map.contains_key(&parts.last().expect("column").value.to_ascii_lowercase())
        }
        Expr::BinaryOp { left, right, .. } => {
            filter_references_select_outputs(left, output_map)
                || filter_references_select_outputs(right, output_map)
        }
        Expr::Nested(expr) => filter_references_select_outputs(expr, output_map),
        _ => false,
    }
}

fn replace_output_column_refs(expr: Expr, output_map: &HashMap<String, Expr>) -> Expr {
    match expr {
        Expr::Identifier(ident) => output_map
            .get(&ident.value.to_ascii_lowercase())
            .cloned()
            .unwrap_or(Expr::Identifier(ident)),
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => output_map
            .get(&parts.last().expect("column").value.to_ascii_lowercase())
            .cloned()
            .unwrap_or(Expr::CompoundIdentifier(parts)),
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(replace_output_column_refs(*left, output_map)),
            op,
            right: Box::new(replace_output_column_refs(*right, output_map)),
        },
        Expr::Nested(inner) => {
            Expr::Nested(Box::new(replace_output_column_refs(*inner, output_map)))
        }
        other => other,
    }
}

pub(crate) fn aggregate_finalize_sql(
    sink_table: &str,
    constraint: &str,
    dimensions: &[String],
) -> Result<(String, String), RewriteError> {
    let constraint_expr = parse_expr(constraint)?;

    if dimensions.is_empty() {
        let finalize = statement_from_query(query_from_select(grouped_select(
            vec![alias_expr(
                Expr::Nested(Box::new(constraint_expr.clone())),
                "constraint_result",
            )],
            vec![TableWithJoins {
                relation: table_factor(sink_table),
                joins: Vec::new(),
            }],
            None,
            Vec::new(),
        )));
        let invalidate = build_finalize_invalidate_update(sink_table, None, constraint_expr, None)?;
        return Ok((finalize.to_string(), invalidate.to_string()));
    }

    let inner_alias = "__passant_group";
    let inner_constraint = rename_table_refs(constraint_expr.clone(), sink_table, inner_alias);
    let dimension_exprs = dimensions
        .iter()
        .map(|dimension| parse_expr(dimension))
        .collect::<Result<Vec<_>, _>>()?;
    let mut projection = dimension_exprs
        .iter()
        .cloned()
        .map(SelectItem::UnnamedExpr)
        .collect::<Vec<_>>();
    projection.push(alias_expr(
        Expr::Nested(Box::new(constraint_expr.clone())),
        "constraint_result",
    ));

    let finalize = statement_from_query(query_from_select(grouped_select(
        projection,
        vec![TableWithJoins {
            relation: table_factor(sink_table),
            joins: Vec::new(),
        }],
        None,
        dimension_exprs.clone(),
    )));

    let invalidate = build_finalize_invalidate_update(
        sink_table,
        Some(inner_alias),
        inner_constraint,
        Some(&dimension_exprs),
    )?;

    Ok((finalize.to_string(), invalidate.to_string()))
}

/// AST-backed fallback when dimensioned finalize SQL cannot be built.
pub(crate) fn aggregate_finalize_sql_fallback(
    sink_table: &str,
    constraint: &str,
) -> (String, String) {
    if let Ok(pair) = aggregate_finalize_sql(sink_table, constraint, &[]) {
        return pair;
    }
    let constraint_expr = parse_expr(constraint).unwrap_or_else(|_| bool_literal(false));
    let finalize = statement_from_query(query_from_select(grouped_select(
        vec![alias_expr(
            Expr::Nested(Box::new(constraint_expr.clone())),
            "constraint_result",
        )],
        vec![TableWithJoins {
            relation: table_factor(sink_table),
            joins: Vec::new(),
        }],
        None,
        Vec::new(),
    )));
    let invalidate = build_finalize_invalidate_update(sink_table, None, constraint_expr, None)
        .map(|statement| statement.to_string())
        .unwrap_or_else(|_| finalize.to_string());
    (finalize.to_string(), invalidate)
}

fn build_finalize_invalidate_update(
    sink_table: &str,
    inner_alias: Option<&str>,
    constraint_expr: Expr,
    dimension_exprs: Option<&[Expr]>,
) -> Result<Statement, RewriteError> {
    let from_relation = if let Some(alias) = inner_alias {
        sqlparser::ast::TableFactor::Table {
            name: object_name(sink_table),
            alias: Some(table_alias(alias)),
            args: None,
            with_hints: Vec::new(),
            version: None,
            with_ordinality: false,
            partitions: Vec::new(),
            json_path: None,
        }
    } else {
        table_factor(sink_table)
    };

    let selection = match (inner_alias, dimension_exprs) {
        (Some(alias), Some(dimensions)) => and_exprs(
            dimensions
                .iter()
                .map(|dimension| {
                    binary_comparison(
                        rename_table_refs(dimension.clone(), sink_table, alias),
                        BinaryOperator::Eq,
                        dimension.clone(),
                    )
                })
                .collect(),
        ),
        _ => None,
    };

    let subquery = query_from_select(grouped_select(
        vec![SelectItem::UnnamedExpr(Expr::Nested(Box::new(
            constraint_expr,
        )))],
        vec![TableWithJoins {
            relation: from_relation,
            joins: Vec::new(),
        }],
        selection,
        Vec::new(),
    ));

    let subquery_expr = Expr::Subquery(Box::new(subquery));
    let valid_expr = parse_expr("COALESCE(valid, true)")?;
    let enforce_expr = if inner_alias.is_some() {
        function_call(
            "COALESCE",
            vec![
                subquery_expr,
                Expr::Value(sqlparser::ast::Value::Boolean(true)),
            ],
        )
    } else {
        subquery_expr
    };
    let assignment_value =
        and_exprs(vec![valid_expr, enforce_expr]).expect("invalidate assignment predicates");

    Ok(Statement::Update {
        table: TableWithJoins {
            relation: table_factor(sink_table),
            joins: Vec::new(),
        },
        assignments: vec![Assignment {
            target: AssignmentTarget::ColumnName(object_name("valid")),
            value: assignment_value,
        }],
        from: None,
        selection: None,
        returning: None,
        or: None,
    })
}

pub(crate) fn rewrite_source_aggregates_for_finalize(
    constraint: &str,
    sources: &[String],
    aggregate_temp_columns: &[(SourceAggregate, String)],
) -> Result<String, RewriteError> {
    let mut expr = parse_expr(constraint)?;
    let mut replacements = Vec::new();
    for aggregate in source_aggregates(constraint, sources)? {
        if let Some((_, temp_name)) = aggregate_temp_columns
            .iter()
            .find(|(candidate, _)| candidate.sql == aggregate.sql)
        {
            let function_name = aggregate_finalize_function_name(&aggregate.function_name);
            replacements.push((
                aggregate.expr.clone(),
                function_call(
                    function_name,
                    vec![Expr::Identifier(sqlparser::ast::Ident::new(temp_name))],
                ),
            ));
        }
    }
    expr = replace_expr_subtrees(expr, &replacements);
    Ok(expr.to_string())
}

fn source_aggregates(
    constraint: &str,
    sources: &[String],
) -> Result<Vec<SourceAggregate>, RewriteError> {
    constraint_aggregates(constraint, sources, None)
}

pub(crate) fn constraint_aggregates(
    constraint: &str,
    sources: &[String],
    sink: Option<&str>,
) -> Result<Vec<SourceAggregate>, RewriteError> {
    let expr = parse_expr(constraint)?;
    let mut aggregates = Vec::new();
    collect_constraint_aggregates(&expr, constraint, sources, sink, &mut aggregates);
    Ok(aggregates)
}

pub(crate) fn policy_aggregate_temp_entries(
    constraint: &str,
    sources: &[String],
    sink: Option<&str>,
) -> Result<Vec<SourceAggregate>, RewriteError> {
    let all = constraint_aggregates(constraint, sources, sink)?;
    let mut ordered = Vec::new();
    for source in sources {
        for aggregate in &all {
            if !aggregate.is_sink_aggregate
                && expr_references_table(&aggregate.expr, source)
                && ordered
                    .iter()
                    .all(|existing: &SourceAggregate| existing.sql != aggregate.sql)
            {
                ordered.push(aggregate.clone());
            }
        }
    }
    for aggregate in &all {
        if !aggregate.is_sink_aggregate
            && ordered
                .iter()
                .all(|existing: &SourceAggregate| existing.sql != aggregate.sql)
        {
            ordered.push(aggregate.clone());
        }
    }
    for aggregate in &all {
        if aggregate.is_sink_aggregate
            && ordered
                .iter()
                .all(|existing: &SourceAggregate| existing.sql != aggregate.sql)
        {
            ordered.push(aggregate.clone());
        }
    }
    Ok(ordered)
}

fn aggregate_sql_from_constraint(constraint: &str, expr: &Expr) -> String {
    let rendered = expr.to_string();
    if constraint.contains(&rendered) {
        return rendered;
    }
    let lower = constraint.to_ascii_lowercase();
    let needle = rendered.to_ascii_lowercase();
    if let Some(start) = lower.find(&needle) {
        return constraint[start..start + rendered.len()].to_string();
    }
    rendered
}

fn collect_constraint_aggregates(
    expr: &Expr,
    constraint: &str,
    sources: &[String],
    sink: Option<&str>,
    aggregates: &mut Vec<SourceAggregate>,
) {
    match expr {
        Expr::Function(function) if is_aggregate_name(&function.name.to_string()) => {
            if let Some(input) = first_function_expr(function) {
                let refs_source = expr_references_any_source(&input, sources)
                    || expr_references_any_source(expr, sources);
                let refs_sink = sink.is_some_and(|sink| expr_references_table(expr, sink));
                if refs_source || refs_sink {
                    aggregates.push(SourceAggregate {
                        sql: aggregate_sql_from_constraint(constraint, expr),
                        function_name: function.name.to_string(),
                        expr: expr.clone(),
                        is_sink_aggregate: refs_sink && !refs_source,
                    });
                }
            }
        }
        Expr::Function(function) => {
            if let FunctionArguments::List(args) = &function.args {
                for arg in &args.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                    | FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(expr),
                        ..
                    }
                    | FunctionArg::ExprNamed {
                        arg: FunctionArgExpr::Expr(expr),
                        ..
                    } = arg
                    {
                        collect_constraint_aggregates(expr, constraint, sources, sink, aggregates);
                    }
                }
            }
            if let Some(filter) = function.filter.as_ref() {
                collect_constraint_aggregates(filter, constraint, sources, sink, aggregates);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_constraint_aggregates(left, constraint, sources, sink, aggregates);
            collect_constraint_aggregates(right, constraint, sources, sink, aggregates);
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => {
            collect_constraint_aggregates(expr, constraint, sources, sink, aggregates)
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                collect_constraint_aggregates(operand, constraint, sources, sink, aggregates);
            }
            for expr in conditions.iter().chain(results.iter()) {
                collect_constraint_aggregates(expr, constraint, sources, sink, aggregates);
            }
            if let Some(else_result) = else_result {
                collect_constraint_aggregates(else_result, constraint, sources, sink, aggregates);
            }
        }
        _ => {}
    }
}

fn expr_references_table(expr: &Expr, table: &str) -> bool {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            parts[0].value.eq_ignore_ascii_case(table)
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_references_table(left, table) || expr_references_table(right, table)
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => expr_references_table(expr, table),
        Expr::Function(function) => {
            if let FunctionArguments::List(args) = &function.args
                && args.args.iter().any(|arg| match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                    | FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(expr),
                        ..
                    }
                    | FunctionArg::ExprNamed {
                        arg: FunctionArgExpr::Expr(expr),
                        ..
                    } => expr_references_table(expr, table),
                    _ => false,
                })
            {
                return true;
            }
            function
                .filter
                .as_deref()
                .is_some_and(|filter| expr_references_table(filter, table))
        }
        _ => false,
    }
}

fn aggregate_has_filter(expr: &Expr) -> bool {
    matches!(expr, Expr::Function(function) if function.filter.is_some())
}

pub(crate) fn expr_is_aggregate(expr: &Expr) -> bool {
    matches!(expr, Expr::Function(function) if is_aggregate_name(&function.name.to_string()))
}

pub(crate) fn aggregate_temp_projection_expr(
    aggregate: &SourceAggregate,
    is_query_aggregation: bool,
    context: Option<&RewriteContext>,
    sink: Option<&str>,
    select: Option<&Select>,
) -> Result<Expr, RewriteError> {
    let preserve_full_aggregate = is_query_aggregation || aggregate_has_filter(&aggregate.expr);

    if aggregate_has_filter(&aggregate.expr)
        && !is_query_aggregation
        && let (Some(context), Some(sink), Some(select)) = (context, sink, select)
        && let Some(case_expr) =
            filter_aggregate_to_case_expr(&aggregate.expr, select, context, sink)?
    {
        return Ok(case_expr);
    }

    let expr = if aggregate_has_filter(&aggregate.expr) {
        parse_expr(&aggregate.sql)?
    } else if aggregate.is_sink_aggregate
        && let (Some(context), Some(sink)) = (context, sink)
    {
        if let Expr::Function(function) = &aggregate.expr
            && let Some(inner) = first_function_expr(function)
        {
            let mapped_inner = replace_sink_columns(inner, sink, &context.sink_expr_by_column);
            let mapped_inner =
                replace_sink_columns(mapped_inner, "_OUTPUT_", &context.sink_expr_by_column);
            if preserve_full_aggregate && expr_is_aggregate(&mapped_inner) {
                mapped_inner
            } else if preserve_full_aggregate {
                replace_sink_columns(
                    replace_sink_columns(
                        parse_expr(&aggregate.sql)?,
                        sink,
                        &context.sink_expr_by_column,
                    ),
                    "_OUTPUT_",
                    &context.sink_expr_by_column,
                )
            } else {
                mapped_inner
            }
        } else if preserve_full_aggregate {
            replace_sink_columns(
                replace_sink_columns(
                    parse_expr(&aggregate.sql)?,
                    sink,
                    &context.sink_expr_by_column,
                ),
                "_OUTPUT_",
                &context.sink_expr_by_column,
            )
        } else {
            parse_expr(&aggregate.sql)?
        }
    } else if preserve_full_aggregate {
        parse_expr(&aggregate.sql)?
    } else if let Expr::Function(function) = &aggregate.expr
        && let Some(inner) = first_function_expr(function)
    {
        inner.clone()
    } else {
        parse_expr(&aggregate.sql)?
    };
    Ok(expr)
}

fn aggregate_finalize_function_name(function_name: &str) -> &str {
    if function_name.eq_ignore_ascii_case("count") {
        "sum"
    } else {
        function_name
    }
}

fn expr_references_any_source(expr: &Expr, sources: &[String]) -> bool {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => sources
            .iter()
            .any(|source| parts[0].value.eq_ignore_ascii_case(source)),
        Expr::BinaryOp { left, right, .. } => {
            expr_references_any_source(left, sources) || expr_references_any_source(right, sources)
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => expr_references_any_source(expr, sources),
        Expr::Function(function) => {
            if let FunctionArguments::List(args) = &function.args {
                return args.args.iter().any(|arg| match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                    | FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(expr),
                        ..
                    }
                    | FunctionArg::ExprNamed {
                        arg: FunctionArgExpr::Expr(expr),
                        ..
                    } => expr_references_any_source(expr, sources),
                    _ => false,
                });
            }
            false
        }
        _ => false,
    }
}
