//! Opaque payload operations for the shared joint-lineage traversal.
//!
//! This module deliberately has no recursive query/factor walker. Scope binding,
//! CTE memoization, derived recursion and public stars are handled by its parent.
use std::collections::HashSet;
use std::ops::ControlFlow;

use crate::aggregate_registry::{AggregateFunctionSource, AggregateRegistry};
use crate::catalog::TableCatalog;
use crate::diagnostics::RewriteError;
use crate::sql::parse_policy_expr_duckdb;
use sqlparser::ast::{
    Expr, FunctionArguments, GroupByExpr, Ident, JoinConstraint, JoinOperator, Query, Select,
    SelectItem, SetExpr, TableFactor, Value, WindowType, visit_expressions,
};

pub(super) const DEPS: &str = "_dfc_intermediate_dependencies";
const EMPTY: &str = "CAST([] AS STRUCT(t VARCHAR, r BIGINT)[])";

pub(super) fn error(message: &str) -> RewriteError {
    RewriteError::unsupported_statement(format!("intermediate lineage: {message}"))
}
fn quoted(name: &str) -> String {
    Ident::with_quote('"', name).to_string()
}
fn column(alias: &str, name: &str) -> String {
    format!("{}.{}", quoted(alias), quoted(name))
}
fn concat(parts: &[String]) -> String {
    if parts.is_empty() {
        EMPTY.into()
    } else {
        format!("list_concat({})", parts.join(", "))
    }
}
fn token(label: &str, identity: &str) -> String {
    let label = Expr::Value(Value::SingleQuotedString(label.into()));
    format!("list_value(struct_pack(t := {label}, r := {identity}))")
}

pub(super) fn validate_query(query: &Query) -> Result<(), RewriteError> {
    if !query.locks.is_empty()
        || !query.limit_by.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
    {
        return Err(error(
            "locking and nonrelational query clauses are unsupported",
        ));
    }
    for expr in query
        .limit
        .iter()
        .chain(query.offset.iter().map(|offset| &offset.value))
        .chain(
            query
                .fetch
                .iter()
                .filter_map(|fetch| fetch.quantity.as_ref()),
        )
    {
        validate(expr, false)?;
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err(error(
            "set operations require a joint intermediate merge boundary",
        ));
    };
    if select.distinct.is_some()
        || select.qualify.is_some()
        || select.top.is_some()
        || select.into.is_some()
        || !select.named_window.is_empty()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.value_table_mode.is_some()
        || select.connect_by.is_some()
    {
        return Err(error(
            "DISTINCT, QUALIFY, named windows and nonrelational SELECT clauses are unsupported",
        ));
    }

    if query.with.as_ref().is_some_and(|with| with.recursive) {
        return Err(error("recursive CTEs require a fixpoint state contract"));
    }
    let mut aliases = HashSet::new();
    for from in &select.from {
        for factor in
            std::iter::once(&from.relation).chain(from.joins.iter().map(|join| &join.relation))
        {
            let alias = validate_factor(factor)?;
            if !aliases.insert(alias.to_ascii_lowercase()) {
                return Err(error("ambiguous relation aliases"));
            }
        }
        for join in &from.joins {
            match &join.join_operator {
                JoinOperator::Inner(JoinConstraint::On(condition)) => validate(condition, false)?,
                JoinOperator::CrossJoin | JoinOperator::Inner(JoinConstraint::None) => (),
                _ => {
                    return Err(error(
                        "nullable, NATURAL, USING and non-inner joins need presence-aware lineage",
                    ));
                }
            }
        }
    }
    // Whole-row expressions can inspect hidden state even in predicates,
    // grouping and ordering; check each user expression in this scope only.
    let mut scoped = select
        .selection
        .iter()
        .chain(select.having.iter())
        .collect::<Vec<_>>();
    if let GroupByExpr::Expressions(exprs, _) = &select.group_by {
        scoped.extend(exprs.iter());
    }
    if let Some(order) = &query.order_by {
        scoped.extend(order.exprs.iter().map(|item| &item.expr));
    }
    scoped.extend(query.limit.iter());
    scoped.extend(query.offset.iter().map(|offset| &offset.value));
    scoped.extend(
        query
            .fetch
            .iter()
            .filter_map(|fetch| fetch.quantity.as_ref()),
    );
    for from in &select.from {
        for join in &from.joins {
            if let JoinOperator::Inner(JoinConstraint::On(expr)) = &join.join_operator {
                scoped.push(expr);
            }
        }
    }
    for expr in scoped {
        if visit_expressions(expr, |node| {
            if let Expr::Identifier(i) = node
                && aliases.contains(&i.value.to_ascii_lowercase())
            {
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        })
        .is_break()
        {
            return Err(error("whole-row relation references are unsupported"));
        }
    }
    Ok(())
}

pub(super) fn validate_order(query: &Query, public_width: usize) -> Result<(), RewriteError> {
    if let Some(order) = &query.order_by {
        for item in &order.exprs {
            validate(&item.expr, false)?;
            if let Expr::Value(Value::Number(n, _)) = &item.expr
                && n.parse::<usize>().is_ok_and(|n| n > public_width)
            {
                return Err(error("ORDER BY position exceeds the logical projection"));
            }
        }
    }
    Ok(())
}

fn validate_factor(factor: &TableFactor) -> Result<String, RewriteError> {
    match factor {
        TableFactor::Table {
            name,
            alias,
            args,
            version,
            ..
        } if args.is_none() && version.is_none() && name.0.len() == 1 => {
            if alias.as_ref().is_some_and(|a| !a.columns.is_empty()) {
                return Err(error("relation column alias lists are unsupported"));
            }
            Ok(alias
                .as_ref()
                .map_or_else(|| name.0[0].value.clone(), |a| a.name.value.clone()))
        }
        TableFactor::Derived {
            lateral: false,
            alias: Some(alias),
            ..
        } => {
            if !alias.columns.is_empty() {
                return Err(error("derived column alias lists are unsupported"));
            }
            Ok(alias.name.value.clone())
        }
        _ => Err(error(
            "only physical tables, CTEs and aliased non-lateral derived tables are supported",
        )),
    }
}

pub(super) fn physical_dependency(
    catalog: &TableCatalog,
    name: &str,
    alias: &Ident,
) -> Result<Expr, RewriteError> {
    let mut dependencies = Vec::new();
    if let Some(binding) = catalog.intermediate_binding(name) {
        if let Some(source) = &binding.source {
            dependencies.push(token(
                &format!("source:{}", source.to_ascii_lowercase()),
                &column(&alias.value, "rowid"),
            ));
        }
        if let Some(state_column) = &binding.state_column {
            dependencies.push(token("state", &column(&alias.value, state_column)));
        }
    }
    parse_policy_expr_duckdb(&concat(&dependencies))
}

pub(super) fn imported_dependency(alias: &Ident) -> Expr {
    Expr::CompoundIdentifier(vec![alias.clone(), Ident::new(DEPS)])
}

pub(super) fn lower_select(
    select: &mut Select,
    registry: &AggregateRegistry,
    dependencies: &[Expr],
    aliases: &HashSet<String>,
    named: bool,
) -> Result<Vec<String>, RewriteError> {
    let mut projections = std::mem::take(&mut select.projection);
    let mut output_columns = Vec::new();
    for projection in &projections {
        let (expr, name) = match projection {
            SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.value.clone())),
            SelectItem::UnnamedExpr(expr) => (
                expr,
                match expr {
                    Expr::Identifier(i) => Some(i.value.clone()),
                    Expr::CompoundIdentifier(ids) => ids.last().map(|i| i.value.clone()),
                    _ => None,
                },
            ),
            _ => unreachable!(),
        };
        validate(expr, true)?;
        let alias_reference = visit_expressions(expr, |node| {
            if let Expr::Identifier(i) = node
                && aliases.contains(&i.value.to_ascii_lowercase())
            {
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        });
        if alias_reference.is_break() {
            return Err(error("whole-row relation references are unsupported"));
        }
        if named && name.is_none() {
            return Err(error(
                "CTE/derived expressions require explicit output aliases",
            ));
        }
        output_columns.push(name.unwrap_or_else(|| expr.to_string()));
    }
    for expr in select.selection.iter().chain(select.having.iter()) {
        validate(expr, false)?;
    }
    let grouped = match &select.group_by {
        GroupByExpr::All(_) => return Err(error("GROUP BY ALL is unsupported")),
        GroupByExpr::Expressions(exprs, _) => {
            for expr in exprs {
                validate(expr, false)?;
            }
            !exprs.is_empty()
        }
    };
    let mut windows = Vec::new();
    let mut aggregate = grouped
        || select
            .having
            .as_ref()
            .is_some_and(|expr| registry.expr_contains_aggregate(expr));
    let check = visit_expressions(&projections, |node| {
        if let Expr::Function(function) = node {
            if let Some(window) = &function.over {
                let name = function.name.to_string().to_ascii_lowercase();
                if !registry
                    .lookup(&name)
                    .is_some_and(|a| a.source == AggregateFunctionSource::Builtin)
                    || function.filter.is_some()
                    || function.null_treatment.is_some()
                {
                    return ControlFlow::Break(error(
                        "navigation/ranking, filtered and unknown windows need a distinct contributor contract",
                    ));
                }
                let WindowType::WindowSpec(spec) = window else {
                    return ControlFlow::Break(error("window specifications must be inline"));
                };
                if spec.window_name.is_some() {
                    return ControlFlow::Break(error("window specifications must be inline"));
                }
                if !windows.contains(window) {
                    windows.push(window.clone());
                }
            } else if registry.is_aggregate_call(function) {
                aggregate = true;
            }
        }
        ControlFlow::Continue(())
    });
    if let ControlFlow::Break(e) = check {
        return Err(e);
    }
    if aggregate && !windows.is_empty() {
        return Err(error(
            "factor grouped window inputs into a CTE before windowing",
        ));
    }
    let base = concat(
        &dependencies
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
    );
    let dependency = if aggregate {
        format!("COALESCE(list_distinct(flatten(list({base}))), {EMPTY})")
    } else if windows.is_empty() {
        base
    } else {
        let mut dependencies = vec![base.clone()];
        for window in windows {
            dependencies.push(format!(
                "COALESCE(flatten(list({base}) OVER {window}), {EMPTY})"
            ));
        }
        format!("list_distinct({})", concat(&dependencies))
    };
    projections.push(SelectItem::ExprWithAlias {
        expr: parse_policy_expr_duckdb(&dependency)?,
        alias: Ident::new(DEPS),
    });
    select.projection = projections;
    Ok(output_columns)
}

pub(super) fn validate(expression: &Expr, window: bool) -> Result<(), RewriteError> {
    let result = visit_expressions(expression, |node| {
        match node {
            Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
                return ControlFlow::Break(error(
                    "expression subqueries require their own lineage boundary",
                ));
            }
            Expr::Identifier(i) if i.value.starts_with("_dfc_intermediate_") => {
                return ControlFlow::Break(error("reserved intermediate metadata name"));
            }
            Expr::CompoundIdentifier(ids)
                if ids
                    .iter()
                    .any(|i| i.value.starts_with("_dfc_intermediate_")) =>
            {
                return ControlFlow::Break(error("reserved intermediate metadata name"));
            }
            Expr::Function(f)
                if matches!(f.args, FunctionArguments::Subquery(_))
                    || matches!(f.parameters, FunctionArguments::Subquery(_)) =>
            {
                return ControlFlow::Break(error(
                    "expression subqueries require their own lineage boundary",
                ));
            }
            Expr::Function(f) if f.name.to_string().eq_ignore_ascii_case("columns") => {
                return ControlFlow::Break(error("dynamic COLUMNS projections are unsupported"));
            }
            Expr::Function(f) if f.over.is_some() && !window => {
                return ControlFlow::Break(error("windows outside the projection are unsupported"));
            }
            _ => (),
        }
        ControlFlow::Continue(())
    });
    match result {
        ControlFlow::Break(e) => Err(e),
        _ => Ok(()),
    }
}
