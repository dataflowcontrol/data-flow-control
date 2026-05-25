//! Policy constraint expression building and transformation.

#![allow(clippy::too_many_arguments)]

use std::collections::HashSet;

use smallvec::SmallVec;
use sqlparser::ast::{
    Assignment, AssignmentTarget, BinaryOperator, Expr, FunctionArg, FunctionArgExpr,
    FunctionArguments, Ident, ObjectName,
};

use crate::catalog::TableCatalog;
use crate::diagnostics::RewriteError;
use crate::identifiers::{AliasByBase, QualifiedColumn, TableKey, TableName};
use crate::policy::{PolicyIr, Resolution};
use crate::policy_store::{CompiledPolicy, PolicyStore};
use crate::rewrite_stats::RewriteStatsCell;
use crate::semiring;
use crate::sql::{count_distinct_eq_one, scalar_subquery};

use super::aggregates::{is_scan_transformable_non_distributive, transform_scan_aggregates};
use super::columns::{replace_sink_columns, rewrite_column_qualifiers};
use super::expr::{
    and_expr, append_invalid_message_expr, bool_literal, expr_contains_aggregate,
    invalidate_message_expr, is_aggregate_name, join_conjuncts, kill_expr, parse_expr,
    resolver_expr,
};
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
    catalog: &TableCatalog,
) -> Result<Expr, RewriteError> {
    let PolicyIr::CompatDfc {
        constraint,
        on_fail,
        ..
    } = policy
    else {
        return Err(RewriteError::unsupported_statement(
            "non-DFC policy cannot be pushed into joins",
        ));
    };
    let scan_ready = constraint_ctx.uses_scan_ready_expr();
    let mut expr = constraint_ctx.scan_policy_base_expr(constraint)?;
    if let Some(alias) = alias {
        rewrite_column_qualifiers(&mut expr, &AliasByBase::single(base, alias));
    }
    expr = scan_policy_expr(
        expr,
        policy.sources(),
        &RewriteContext::default(),
        &AliasByBase::default(),
        scan_ready,
    )?;
    if let Some(guard) = unique_column_guard_from_constraint(constraint, catalog, constraint_ctx) {
        expr = and_expr(guard, expr);
    }
    if *on_fail == Resolution::Kill {
        expr = kill_expr(expr)?;
    } else if *on_fail == Resolution::Llm {
        expr = resolver_expr(expr)?;
    }
    Ok(expr)
}

pub(crate) fn scan_policy_expr(
    mut expr: Expr,
    sources: &[String],
    context: &RewriteContext,
    alias_by_base: &AliasByBase,
    scan_ready: bool,
) -> Result<Expr, RewriteError> {
    let non_distributive = non_distributive_aggregates(&expr)?;
    if non_distributive.is_empty() {
        if scan_ready {
            return Ok(expr);
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
    let aggregates = semiring::analyze_constraint(&expr.to_string()).map_err(|err| {
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
pub(crate) fn build_compat_dfc_filter_expr(
    sources: &[String],
    constraint: &str,
    sink_alias: &Option<String>,
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
                    build_single_alias_compat_dfc_filter_expr(
                        constraint,
                        sink_alias,
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
        return build_single_alias_compat_dfc_filter_expr(
            constraint,
            sink_alias,
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
    if let Some(sink) = &context.sink {
        expr = replace_sink_columns(expr, sink, &context.sink_expr_by_column);
        expr = replace_sink_columns(expr, "_OUTPUT_", &context.sink_expr_by_column);
        if let Some(sink_alias) = sink_alias {
            expr = replace_sink_columns(expr, sink_alias, &context.sink_expr_by_column);
        }
    }
    rewrite_column_qualifiers(&mut expr, &table_scope.alias_by_base);
    if !is_aggregation {
        expr = scan_policy_expr(
            expr,
            sources,
            context,
            &table_scope.alias_by_base,
            scan_ready,
        )?;
    }
    Ok(expr)
}

pub(crate) fn unique_column_guard_from_constraint(
    constraint: &str,
    catalog: &TableCatalog,
    constraint_ctx: &ConstraintExprCtx<'_>,
) -> Option<Expr> {
    let expr = constraint_ctx.expr(constraint).ok()?;
    let column = extract_simple_column_comparison(&expr)?;
    if !catalog.is_unique_column(column.table.as_str(), column.column.as_str()) {
        return None;
    }
    Some(count_distinct_eq_one(
        column.table.as_str(),
        column.column.as_str(),
    ))
}

fn extract_simple_column_comparison(expr: &Expr) -> Option<QualifiedColumn> {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq | BinaryOperator::NotEq,
            right,
        } => QualifiedColumn::from_expr(left).or_else(|| QualifiedColumn::from_expr(right)),
        _ => None,
    }
}

pub(crate) fn build_pgn_over_filter_expr(
    sources: &[String],
    constraint: &str,
    sink_alias: &Option<String>,
    applicability: PolicyApplicability,
    context: &RewriteContext,
    table_scope: &TableScope,
    constraint_ctx: &ConstraintExprCtx<'_>,
) -> Result<Expr, RewriteError> {
    if applicability == PolicyApplicability::RequiredSourceMissing {
        return Ok(bool_literal(false));
    }
    let mut expr = constraint_ctx.expr(constraint)?;
    if let Some(sink) = &context.sink {
        expr = replace_sink_columns(expr, sink, &context.sink_expr_by_column);
        expr = replace_sink_columns(expr, "_OUTPUT_", &context.sink_expr_by_column);
        if let Some(sink_alias) = sink_alias {
            expr = replace_sink_columns(expr, sink_alias, &context.sink_expr_by_column);
        }
    }
    rewrite_column_qualifiers(&mut expr, &table_scope.alias_by_base);
    let _ = sources;
    Ok(expr)
}

fn upsert_valid_assignment(assignments: &mut Vec<Assignment>, value: Expr) {
    for assignment in assignments.iter_mut() {
        let AssignmentTarget::ColumnName(name) = &assignment.target else {
            continue;
        };
        if name
            .0
            .last()
            .is_some_and(|ident| ident.value.eq_ignore_ascii_case("valid"))
        {
            assignment.value = and_expr(assignment.value.clone(), value);
            return;
        }
    }

    assignments.push(Assignment {
        target: AssignmentTarget::ColumnName(ObjectName(vec![Ident::new("valid")])),
        value,
    });
}

fn upsert_invalid_string_assignment(
    assignments: &mut Vec<Assignment>,
    value: Expr,
    description: Option<&str>,
) -> Result<(), RewriteError> {
    for assignment in assignments.iter_mut() {
        let AssignmentTarget::ColumnName(name) = &assignment.target else {
            continue;
        };
        if name
            .0
            .last()
            .is_some_and(|ident| ident.value.eq_ignore_ascii_case("invalid_string"))
        {
            assignment.value =
                append_invalid_message_expr(assignment.value.clone(), value, description)?;
            return Ok(());
        }
    }

    assignments.push(Assignment {
        target: AssignmentTarget::ColumnName(ObjectName(vec![Ident::new("invalid_string")])),
        value: invalidate_message_expr(value, description)?,
    });
    Ok(())
}

pub(crate) fn apply_update_resolution(
    assignments: &mut Vec<Assignment>,
    selection: &mut Option<Expr>,
    expr: Expr,
    on_fail: Resolution,
    description: Option<&str>,
) -> Result<(), RewriteError> {
    match on_fail {
        Resolution::Remove => {
            *selection = Some(match selection.take() {
                Some(existing) => and_expr(existing, expr),
                None => expr,
            });
        }
        Resolution::Kill => {
            let expr = kill_expr(expr)?;
            *selection = Some(match selection.take() {
                Some(existing) => and_expr(existing, expr),
                None => expr,
            });
        }
        Resolution::Invalidate => {
            upsert_valid_assignment(assignments, expr);
        }
        Resolution::InvalidateMessage => {
            upsert_invalid_string_assignment(assignments, expr, description)?;
        }
        Resolution::Llm => {
            let expr = resolver_expr(expr)?;
            *selection = Some(match selection.take() {
                Some(existing) => and_expr(existing, expr),
                None => expr,
            });
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_single_alias_compat_dfc_filter_expr(
    constraint: &str,
    sink_alias: &Option<String>,
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
    if let Some(sink) = &context.sink {
        expr = replace_sink_columns(expr, sink, &context.sink_expr_by_column);
        expr = replace_sink_columns(expr, "_OUTPUT_", &context.sink_expr_by_column);
        if let Some(sink_alias) = sink_alias {
            expr = replace_sink_columns(expr, sink_alias, &context.sink_expr_by_column);
        }
    }
    rewrite_column_qualifiers(&mut expr, &alias_map);
    if !is_aggregation {
        expr = scan_policy_expr(expr, sources, context, &alias_map, scan_ready)?;
    }
    Ok(expr)
}

pub(crate) fn build_invalidate_projection_expr(
    sources: &[String],
    constraint: &str,
    sink_alias: &Option<String>,
    applicability: PolicyApplicability,
    context: &RewriteContext,
    table_scope: &TableScope,
    constraint_ctx: &ConstraintExprCtx<'_>,
) -> Result<Expr, RewriteError> {
    if applicability == PolicyApplicability::RequiredSourceMissing {
        return Ok(bool_literal(false));
    }
    let mut expr = constraint_ctx.expr(constraint)?;
    if let Some(sink) = &context.sink {
        expr = replace_sink_columns(expr, sink, &context.sink_expr_by_column);
        expr = replace_sink_columns(expr, "_OUTPUT_", &context.sink_expr_by_column);
        if let Some(sink_alias) = sink_alias {
            expr = replace_sink_columns(expr, sink_alias, &context.sink_expr_by_column);
        }
    }
    rewrite_column_qualifiers(&mut expr, &table_scope.alias_by_base);
    let _ = sources;
    Ok(expr)
}
