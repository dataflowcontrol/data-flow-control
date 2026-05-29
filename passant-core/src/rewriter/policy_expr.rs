//! Policy constraint expression building and transformation.

#![allow(clippy::too_many_arguments)]

use std::collections::{HashMap, HashSet};

use smallvec::SmallVec;
use sqlparser::ast::{
    Assignment, BinaryOperator, DuplicateTreatment, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments,
};

use crate::catalog::TableCatalog;
use crate::diagnostics::RewriteError;
use crate::identifiers::{AliasByBase, QualifiedColumn, TableKey, TableName};
use crate::policy::{PolicyIr, Resolution};
use crate::policy_store::{CompiledPolicy, PolicyStore};
use crate::rewrite_stats::RewriteStatsCell;
use crate::semiring;
use crate::sql::{
    and_exprs, count_distinct_eq_one, max_column, min_column, passant_kill_pass_filter,
    scalar_subquery,
};

use super::aggregates::{is_scan_transformable_non_distributive, transform_scan_aggregates};
use super::columns::{
    apply_output_marker_replacements, apply_policy_sink_column_replacements,
    replace_source_alias_qualifiers, rewrite_column_qualifiers,
};
use super::expr::{
    and_expr, bool_literal, expr_contains_aggregate, is_aggregate_name, join_conjuncts, parse_expr,
};
use super::resolution::PASSANT_KILL_UDF;
use super::scope::TableScope;
use super::types::{PolicyApplicability, RewriteContext};

/// Registration-time constraint AST lookup for rewrite hot paths.
pub(crate) struct ConstraintExprCtx<'a> {
    pub store: &'a PolicyStore,
    pub index: usize,
    pub stats: Option<&'a RewriteStatsCell>,
}

impl ConstraintExprCtx<'_> {
    pub fn expr(&self, constraint: &str) -> Result<Expr, RewriteError> {
        if let Some(ast) = self.store.clone_constraint_ast(self.index) {
            return Ok(ast);
        }
        if let Some(stats) = self.stats {
            stats.record_constraint_parse();
        }
        parse_expr(constraint)
    }

    pub fn scan_policy_base_expr(&self, constraint: &str) -> Result<Expr, RewriteError> {
        if let Some(expr) = self.store.scan_ready_expr(self.index) {
            return Ok(expr);
        }
        self.expr(constraint)
    }

    pub fn uses_scan_ready_expr(&self) -> bool {
        self.store.scan_ready_expr(self.index).is_some()
    }
}

fn referenced_source_tables(expr: &Expr, sources: &[String]) -> Vec<String> {
    let source_keys = sources
        .iter()
        .map(|source| source.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let mut referenced = HashSet::new();
    collect_referenced_source_tables(expr, &source_keys, &mut referenced);
    let mut referenced = referenced.into_iter().collect::<Vec<_>>();
    referenced.sort();
    referenced
}

fn collect_referenced_source_tables(
    expr: &Expr,
    source_keys: &HashSet<String>,
    referenced: &mut HashSet<String>,
) {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            let table = parts[0].value.to_ascii_lowercase();
            if source_keys.contains(&table) {
                referenced.insert(table);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_referenced_source_tables(left, source_keys, referenced);
            collect_referenced_source_tables(right, source_keys, referenced);
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => {
            collect_referenced_source_tables(expr, source_keys, referenced);
        }
        Expr::Function(function) => {
            collect_function_referenced_source_tables(function, source_keys, referenced);
            if let Some(filter) = function.filter.as_ref() {
                collect_referenced_source_tables(filter, source_keys, referenced);
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_referenced_source_tables(expr, source_keys, referenced);
            collect_referenced_source_tables(low, source_keys, referenced);
            collect_referenced_source_tables(high, source_keys, referenced);
        }
        Expr::InList { expr, list, .. } => {
            collect_referenced_source_tables(expr, source_keys, referenced);
            for item in list {
                collect_referenced_source_tables(item, source_keys, referenced);
            }
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                collect_referenced_source_tables(operand, source_keys, referenced);
            }
            for expr in conditions.iter().chain(results.iter()) {
                collect_referenced_source_tables(expr, source_keys, referenced);
            }
            if let Some(else_result) = else_result {
                collect_referenced_source_tables(else_result, source_keys, referenced);
            }
        }
        _ => {}
    }
}

fn collect_function_referenced_source_tables(
    function: &sqlparser::ast::Function,
    source_keys: &HashSet<String>,
    referenced: &mut HashSet<String>,
) {
    let FunctionArguments::List(args) = &function.args else {
        return;
    };
    for arg in &args.args {
        match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
            | FunctionArg::Named {
                arg: FunctionArgExpr::Expr(expr),
                ..
            }
            | FunctionArg::ExprNamed {
                arg: FunctionArgExpr::Expr(expr),
                ..
            } => collect_referenced_source_tables(expr, source_keys, referenced),
            _ => {}
        }
    }
}

fn expr_is_aggregate_only(expr: &Expr) -> bool {
    !expr_has_column_outside_aggregate(expr, false)
}

fn expr_has_column_outside_aggregate(expr: &Expr, inside_aggregate: bool) -> bool {
    match expr {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => !inside_aggregate,
        Expr::Function(function) => {
            let inside_aggregate =
                inside_aggregate || is_aggregate_name(&function.name.to_string());
            function_args_have_column_outside_aggregate(function, inside_aggregate)
                || function
                    .filter
                    .as_ref()
                    .is_some_and(|filter| expr_has_column_outside_aggregate(filter, false))
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_has_column_outside_aggregate(left, inside_aggregate)
                || expr_has_column_outside_aggregate(right, inside_aggregate)
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => expr_has_column_outside_aggregate(expr, inside_aggregate),
        Expr::Between {
            expr, low, high, ..
        } => {
            expr_has_column_outside_aggregate(expr, inside_aggregate)
                || expr_has_column_outside_aggregate(low, inside_aggregate)
                || expr_has_column_outside_aggregate(high, inside_aggregate)
        }
        Expr::InList { expr, list, .. } => {
            expr_has_column_outside_aggregate(expr, inside_aggregate)
                || list
                    .iter()
                    .any(|expr| expr_has_column_outside_aggregate(expr, inside_aggregate))
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            operand
                .as_deref()
                .is_some_and(|expr| expr_has_column_outside_aggregate(expr, inside_aggregate))
                || conditions
                    .iter()
                    .any(|expr| expr_has_column_outside_aggregate(expr, inside_aggregate))
                || results
                    .iter()
                    .any(|expr| expr_has_column_outside_aggregate(expr, inside_aggregate))
                || else_result
                    .as_deref()
                    .is_some_and(|expr| expr_has_column_outside_aggregate(expr, inside_aggregate))
        }
        _ => false,
    }
}

fn function_args_have_column_outside_aggregate(
    function: &sqlparser::ast::Function,
    inside_aggregate: bool,
) -> bool {
    let FunctionArguments::List(args) = &function.args else {
        return false;
    };
    args.args.iter().any(|arg| match arg {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
        | FunctionArg::Named {
            arg: FunctionArgExpr::Expr(expr),
            ..
        }
        | FunctionArg::ExprNamed {
            arg: FunctionArgExpr::Expr(expr),
            ..
        } => expr_has_column_outside_aggregate(expr, inside_aggregate),
        _ => false,
    })
}

fn inverse_alias_map(alias_by_base: &AliasByBase) -> AliasByBase {
    alias_by_base.inverted()
}

pub(crate) fn join_pushdown_expr(
    policy: &PolicyIr,
    constraint_ctx: &ConstraintExprCtx<'_>,
    base: &str,
    alias: Option<String>,
    _catalog: &TableCatalog,
    context: &RewriteContext,
) -> Result<Expr, RewriteError> {
    let PolicyIr::Pgn {
        constraint,
        sink_alias,
        source_aliases,
        ..
    } = policy;
    let scan_ready = constraint_ctx.uses_scan_ready_expr();
    let mut expr = constraint_ctx.scan_policy_base_expr(constraint)?;
    expr = replace_source_alias_qualifiers(expr, source_aliases);
    if let Some(sink) = &context.sink {
        expr = apply_policy_sink_column_replacements(
            expr,
            sink,
            sink_alias,
            policy.sources(),
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
    let alias_by_base = alias
        .as_ref()
        .map(|table_alias| AliasByBase::single(base, table_alias.clone()));
    if let Some(alias_map) = &alias_by_base {
        rewrite_column_qualifiers(&mut expr, alias_map);
    }
    expr = scan_policy_expr(
        expr,
        policy.sources(),
        context,
        &AliasByBase::default(),
        scan_ready,
        false,
    )?;
    if let Some(guard) =
        unique_column_guard_from_constraint(constraint, policy.sources(), constraint_ctx)
    {
        let mut guard = guard;
        if let Some(alias_map) = &alias_by_base {
            rewrite_column_qualifiers(&mut guard, alias_map);
        }
        let guard = scalar_policy_subquery_expr(guard, policy.sources())?;
        expr = and_expr(guard, expr);
    }
    Ok(expr)
}

fn function_is_distinct(function: &Function) -> bool {
    match &function.args {
        FunctionArguments::List(list) => {
            list.duplicate_treatment == Some(DuplicateTreatment::Distinct)
        }
        _ => false,
    }
}

pub(crate) fn is_count_distinct_cardinality_one_check(expr: &Expr) -> bool {
    if let Expr::Nested(inner) = expr {
        return is_count_distinct_cardinality_one_check(inner);
    }
    let Expr::BinaryOp { left, op, right } = expr else {
        return false;
    };
    if !matches!(
        op,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Gt
            | BinaryOperator::Lt
            | BinaryOperator::GtEq
            | BinaryOperator::LtEq
    ) {
        return false;
    }
    let Expr::Function(function) = left.as_ref() else {
        return false;
    };
    if !function.name.to_string().eq_ignore_ascii_case("count") {
        return false;
    }
    if !function_is_distinct(function) {
        return false;
    }
    matches!(
        right.as_ref(),
        Expr::Value(sqlparser::ast::Value::Number(value, _))
            if value == "1"
    )
}

fn count_distinct_cardinality_operator(expr: &Expr) -> Option<BinaryOperator> {
    let target = match expr {
        Expr::Nested(inner) => inner.as_ref(),
        other => other,
    };
    let Expr::BinaryOp { op, .. } = target else {
        return None;
    };
    Some(op.clone())
}

pub(crate) fn is_count_distinct_not_unique_check(expr: &Expr) -> bool {
    is_count_distinct_cardinality_one_check(expr)
        && matches!(
            count_distinct_cardinality_operator(expr),
            Some(BinaryOperator::NotEq)
        )
}

fn is_count_distinct_threshold_comparison(expr: &Expr) -> bool {
    let Expr::BinaryOp { left, op, right } = expr else {
        return false;
    };
    if !matches!(
        op,
        BinaryOperator::Gt | BinaryOperator::Lt | BinaryOperator::GtEq | BinaryOperator::LtEq
    ) {
        return false;
    }
    let Expr::Function(function) = left.as_ref() else {
        return false;
    };
    if !function.name.to_string().eq_ignore_ascii_case("count") {
        return false;
    }
    matches!(right.as_ref(), Expr::Value(_)) && function_is_distinct(function)
}

pub(crate) fn scan_policy_expr(
    mut expr: Expr,
    sources: &[String],
    context: &RewriteContext,
    alias_by_base: &AliasByBase,
    scan_ready: bool,
    is_aggregation: bool,
) -> Result<Expr, RewriteError> {
    if let Expr::BinaryOp {
        left,
        op: BinaryOperator::And,
        right,
    } = &expr
    {
        return Ok(and_expr(
            scan_policy_expr(
                *left.clone(),
                sources,
                context,
                alias_by_base,
                scan_ready,
                is_aggregation,
            )?,
            scan_policy_expr(
                *right.clone(),
                sources,
                context,
                alias_by_base,
                scan_ready,
                is_aggregation,
            )?,
        ));
    }
    let non_distributive = non_distributive_aggregates(&expr)?;
    if non_distributive.is_empty() {
        if scan_ready {
            if context.sink.is_none() && !sources.is_empty() {
                if let Expr::BinaryOp {
                    left,
                    op: BinaryOperator::Or,
                    right,
                } = &expr
                {
                    return Ok(Expr::BinaryOp {
                        left: Box::new(scan_policy_expr(
                            *left.clone(),
                            sources,
                            context,
                            alias_by_base,
                            false,
                            is_aggregation,
                        )?),
                        op: BinaryOperator::Or,
                        right: Box::new(scan_policy_expr(
                            *right.clone(),
                            sources,
                            context,
                            alias_by_base,
                            false,
                            is_aggregation,
                        )?),
                    });
                }
                if expr_is_aggregate_only(&expr) && expr_contains_aggregate(&expr) {
                    rewrite_column_qualifiers(&mut expr, &inverse_alias_map(alias_by_base));
                    return scalar_policy_subquery_expr(expr, sources);
                }
            }
            return Ok(expr);
        }
        if context.sink.is_none() && !sources.is_empty() {
            if let Expr::BinaryOp {
                left,
                op: BinaryOperator::Or,
                right,
            } = &expr
            {
                return Ok(Expr::BinaryOp {
                    left: Box::new(scan_policy_expr(
                        *left.clone(),
                        sources,
                        context,
                        alias_by_base,
                        scan_ready,
                        is_aggregation,
                    )?),
                    op: BinaryOperator::Or,
                    right: Box::new(scan_policy_expr(
                        *right.clone(),
                        sources,
                        context,
                        alias_by_base,
                        scan_ready,
                        is_aggregation,
                    )?),
                });
            }
            if expr_is_aggregate_only(&expr) && expr_contains_aggregate(&expr) {
                if is_count_distinct_threshold_comparison(&expr) {
                    return transform_scan_aggregates(expr);
                }
                if is_count_distinct_not_unique_check(&expr)
                    && context.sink.is_none()
                    && !is_aggregation
                {
                    // Per-tuple provenance on a non-aggregated scan always has one distinct
                    // source value; NOT UNIQUE is false until the query is grouped.
                    return Ok(bool_literal(false));
                }
                rewrite_column_qualifiers(&mut expr, &inverse_alias_map(alias_by_base));
                return scalar_policy_subquery_expr(expr, sources);
            }
        }
        return transform_scan_aggregates(expr);
    }
    if context.sink.is_none() && !sources.is_empty() && expr_is_aggregate_only(&expr) {
        if non_distributive
            .iter()
            .all(|aggregate| is_scan_transformable_non_distributive(aggregate))
        {
            let transformed = transform_scan_aggregates(expr.clone())?;
            if !expr_contains_aggregate(&transformed) {
                let mut transformed = transformed;
                rewrite_column_qualifiers(&mut transformed, &inverse_alias_map(alias_by_base));
                return Ok(transformed);
            }
        }
        rewrite_column_qualifiers(&mut expr, &inverse_alias_map(alias_by_base));
        return scalar_policy_subquery_expr(expr, sources);
    }
    Err(RewriteError::unsupported_statement(format!(
        "non-distributive policy aggregate(s) require Partial-Push: {}",
        non_distributive.join(", ")
    )))
}

pub(crate) fn non_distributive_aggregates(expr: &Expr) -> Result<Vec<String>, RewriteError> {
    let aggregates =
        semiring::analyze_constraint(&crate::sql::render_expr(expr, None)).map_err(|err| {
            RewriteError::unsupported_statement(format!("policy aggregate analysis: {err}"))
        })?;
    Ok(aggregates
        .into_iter()
        .filter(|aggregate| !aggregate.distributive)
        .map(|aggregate| aggregate.expression)
        .collect::<Vec<_>>())
}

fn scalar_policy_subquery_expr(expr: Expr, sources: &[String]) -> Result<Expr, RewriteError> {
    if let Expr::BinaryOp {
        left,
        op: BinaryOperator::And,
        right,
    } = expr
    {
        return Ok(and_expr(
            scalar_policy_subquery_expr(*left, sources)?,
            scalar_policy_subquery_expr(*right, sources)?,
        ));
    }

    if let Expr::BinaryOp { left, op, right } = &expr
        && matches!(
            op,
            BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Gt
                | BinaryOperator::Lt
                | BinaryOperator::GtEq
                | BinaryOperator::LtEq
        )
    {
        let left_sources = referenced_source_tables(left, sources);
        let right_sources = referenced_source_tables(right, sources);
        if left_sources.len() == 1 && right_sources.len() == 1 {
            return Ok(Expr::BinaryOp {
                left: Box::new(scalar_subquery(*left.clone(), &left_sources[0])),
                op: op.clone(),
                right: Box::new(scalar_subquery(*right.clone(), &right_sources[0])),
            });
        }
    }

    let referenced_sources = referenced_source_tables(&expr, sources);
    let source = if referenced_sources.len() == 1 {
        referenced_sources[0].clone()
    } else if referenced_sources.is_empty() && sources.len() == 1 {
        sources[0].clone()
    } else {
        return Err(RewriteError::unsupported_statement(
            "non-distributive multi-source aggregate predicate requires Partial-Push",
        ));
    };
    Ok(scalar_subquery(expr, &source))
}

pub(crate) fn compiled_policy_applicability(
    compiled: &CompiledPolicy,
    tables: &HashSet<TableKey>,
    sink: Option<&TableKey>,
    allow_partial_source_visibility: bool,
) -> Option<PolicyApplicability> {
    policy_applicability_from_keys(
        &compiled.policy,
        tables,
        sink,
        &compiled.source_keys,
        &compiled.required_source_keys,
        allow_partial_source_visibility,
    )
}

fn policy_applicability_from_keys(
    policy: &PolicyIr,
    tables: &HashSet<TableKey>,
    sink: Option<&TableKey>,
    source_keys: &SmallVec<[TableKey; 4]>,
    required_source_keys: &SmallVec<[TableKey; 4]>,
    allow_partial_source_visibility: bool,
) -> Option<PolicyApplicability> {
    let sink_matches = match policy.sink() {
        Some(_) => compiled_sink_matches(policy, sink),
        None => true,
    };
    if !sink_matches {
        return None;
    }

    if policy.sink().is_some() {
        let required_sources = required_source_keys.iter().collect::<HashSet<_>>();
        let non_required_sources_match = source_keys
            .iter()
            .all(|source_key| required_sources.contains(source_key) || tables.contains(source_key));
        if !non_required_sources_match {
            return None;
        }
        if required_source_keys
            .iter()
            .any(|source| !tables.contains(source))
        {
            return Some(PolicyApplicability::RequiredSourceMissing);
        }
        return Some(PolicyApplicability::Normal);
    }

    source_keys
        .iter()
        .all(|source| tables.contains(source))
        .then_some(PolicyApplicability::Normal)
        .or_else(|| {
            if allow_partial_source_visibility
                && policy.sink().is_none()
                && required_source_keys.is_empty()
                && source_keys.len() > 1
                && source_keys.iter().any(|source| tables.contains(source))
            {
                Some(PolicyApplicability::Normal)
            } else {
                None
            }
        })
}

fn compiled_sink_matches(policy: &PolicyIr, sink: Option<&TableKey>) -> bool {
    let Some(policy_sink) = policy.sink() else {
        return false;
    };
    sink.is_some_and(|query_sink| query_sink.as_str() == TableKey::new(policy_sink).as_str())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_pgn_filter_expr(
    sources: &[String],
    constraint: &str,
    sink_alias: &Option<String>,
    source_aliases: &HashMap<String, String>,
    applicability: PolicyApplicability,
    context: &RewriteContext,
    table_scope: &TableScope,
    is_aggregation: bool,
    constraint_ctx: &ConstraintExprCtx<'_>,
) -> Result<Expr, RewriteError> {
    if applicability == PolicyApplicability::RequiredSourceMissing {
        return Ok(bool_literal(false));
    }
    if sources.len() == 1 {
        let base = sources[0].to_ascii_lowercase();
        if let Some(aliases) = table_scope.aliases_by_base.get(&base)
            && aliases.len() > 1
        {
            let filters = aliases
                .iter()
                .map(|alias| {
                    build_single_alias_pgn_filter_expr(
                        constraint,
                        sink_alias,
                        source_aliases,
                        context,
                        sources,
                        &base,
                        alias,
                        is_aggregation,
                        constraint_ctx,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(join_conjuncts(filters));
        }
        return build_single_alias_pgn_filter_expr(
            constraint,
            sink_alias,
            source_aliases,
            context,
            sources,
            &base,
            table_scope
                .alias_for(&TableName::parse(&base))
                .unwrap_or(sources[0].as_str()),
            is_aggregation,
            constraint_ctx,
        );
    }
    let scan_ready = !is_aggregation && constraint_ctx.uses_scan_ready_expr();
    let mut expr = if is_aggregation {
        constraint_ctx.expr(constraint)?
    } else {
        constraint_ctx.scan_policy_base_expr(constraint)?
    };
    expr = replace_source_alias_qualifiers(expr, source_aliases);
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
    rewrite_column_qualifiers(&mut expr, &table_scope.alias_by_base);
    if !is_aggregation {
        expr = scan_policy_expr(
            expr,
            sources,
            context,
            &table_scope.alias_by_base,
            scan_ready,
            false,
        )?;
        rewrite_column_qualifiers(&mut expr, &table_scope.alias_by_base);
    }
    Ok(expr)
}

pub(crate) fn unique_column_guard_from_constraint(
    constraint: &str,
    sources: &[String],
    constraint_ctx: &ConstraintExprCtx<'_>,
) -> Option<Expr> {
    let expr = constraint_ctx.expr(constraint).ok()?;
    let comparison = extract_column_value_comparison(&expr)?;
    let table = comparison.column.table.as_str();
    if !sources
        .iter()
        .any(|source| source.eq_ignore_ascii_case(table))
    {
        return None;
    }
    let column = comparison.column.column.as_str();
    let ColumnValueComparison { op, other, .. } = comparison;
    and_exprs(vec![
        count_distinct_eq_one(table, column),
        Expr::BinaryOp {
            left: Box::new(min_column(table, column)),
            op: op.clone(),
            right: Box::new(other.clone()),
        },
        Expr::BinaryOp {
            left: Box::new(max_column(table, column)),
            op,
            right: Box::new(other),
        },
    ])
}

struct ColumnValueComparison {
    column: QualifiedColumn,
    op: BinaryOperator,
    other: Expr,
}

fn extract_column_value_comparison(expr: &Expr) -> Option<ColumnValueComparison> {
    let Expr::BinaryOp { left, op, right } = expr else {
        return None;
    };
    if !matches!(
        op,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Gt
            | BinaryOperator::Lt
            | BinaryOperator::GtEq
            | BinaryOperator::LtEq
    ) {
        return None;
    }
    if let Some(column) = QualifiedColumn::from_expr(left) {
        if QualifiedColumn::from_expr(right).is_some() {
            return None;
        }
        return Some(ColumnValueComparison {
            column,
            op: op.clone(),
            other: right.as_ref().clone(),
        });
    }
    if let Some(column) = QualifiedColumn::from_expr(right) {
        if QualifiedColumn::from_expr(left).is_some() {
            return None;
        }
        return Some(ColumnValueComparison {
            column,
            op: op.clone(),
            other: left.as_ref().clone(),
        });
    }
    None
}

pub(crate) fn update_tuple_udf_pass_filter(
    pass_expr: Expr,
    udf_name: &str,
) -> Result<Expr, RewriteError> {
    if udf_name.eq_ignore_ascii_case(PASSANT_KILL_UDF) || udf_name.eq_ignore_ascii_case("kill") {
        Ok(passant_kill_pass_filter(pass_expr))
    } else {
        Err(RewriteError::unsupported_statement(format!(
            "tuple UDF resolution '{udf_name}' on UPDATE is not supported yet; \
             use ON FAIL REMOVE or KILL"
        )))
    }
}

pub(crate) fn apply_update_resolution(
    _assignments: &mut [Assignment],
    selection: &mut Option<Expr>,
    expr: Expr,
    on_fail: Resolution,
    _description: Option<&str>,
) -> Result<(), RewriteError> {
    match on_fail {
        Resolution::Remove => {
            *selection = Some(match selection.take() {
                Some(existing) => and_expr(existing, expr),
                None => expr,
            });
        }
        Resolution::Kill => {
            let filter = update_tuple_udf_pass_filter(expr, PASSANT_KILL_UDF)?;
            *selection = Some(match selection.take() {
                Some(existing) => and_expr(existing, filter),
                None => filter,
            });
        }
        Resolution::Udf(name) => {
            let filter = update_tuple_udf_pass_filter(expr, &name)?;
            *selection = Some(match selection.take() {
                Some(existing) => and_expr(existing, filter),
                None => filter,
            });
        }
        Resolution::RelationUdf(_) => {
            return Err(RewriteError::unsupported_statement(
                "relation UDF resolution on UPDATE is not supported yet",
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_single_alias_pgn_filter_expr(
    constraint: &str,
    sink_alias: &Option<String>,
    source_aliases: &HashMap<String, String>,
    context: &RewriteContext,
    sources: &[String],
    base: &str,
    alias: &str,
    is_aggregation: bool,
    constraint_ctx: &ConstraintExprCtx<'_>,
) -> Result<Expr, RewriteError> {
    let alias_map = AliasByBase::single(base, alias);
    let scan_ready = !is_aggregation && constraint_ctx.uses_scan_ready_expr();
    let mut expr = if is_aggregation {
        constraint_ctx.expr(constraint)?
    } else {
        constraint_ctx.scan_policy_base_expr(constraint)?
    };
    expr = replace_source_alias_qualifiers(expr, source_aliases);
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
    rewrite_column_qualifiers(&mut expr, &alias_map);
    if !is_aggregation {
        expr = scan_policy_expr(
            expr,
            sources,
            context,
            &alias_map,
            scan_ready,
            is_aggregation,
        )?;
        rewrite_column_qualifiers(&mut expr, &alias_map);
    }
    Ok(expr)
}
