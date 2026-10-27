//! Defer INSERT policies that combine source aggregates or projected output columns
//! until the aggregation CTE and outer SELECT body.

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use sqlparser::ast::{
    Expr, Ident, Query, Select, SelectItem, SetExpr, TableFactor, visit_expressions,
    visit_expressions_mut,
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
use super::helpers::{OUTPUT_MARKER, OutputColumnMapping};
use super::policy_expr::ConstraintExprCtx;
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
    catalog: &TableCatalog,
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
    let mut aggregate_projections = Vec::new();
    let mut used_aliases = HashSet::new();
    for cte in &with.cte_tables {
        if let SetExpr::Select(select) = cte.query.body.as_ref() {
            for item in &select.projection {
                if let Some((_, Some(name))) = projection_expr_and_name(item) {
                    used_aliases.insert(name.to_ascii_lowercase());
                }
            }
        }
    }
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
        if policy_sink.as_deref().is_some_and(|policy_sink| {
            sink.is_none_or(|sink| TableKey::new(policy_sink) != TableKey::new(sink))
        }) {
            continue;
        }
        if !matches!(
            on_fail,
            Resolution::Kill | Resolution::Udf(_) | Resolution::RelationUdf(_)
        ) {
            continue;
        }
        if !sources.is_empty()
            && !policy_needs_insert_output_scope(store, index, policy, constraint, output_mapping)?
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
        if aggregation_cte
            .as_ref()
            .is_some_and(|name| name != &cte_name)
        {
            return Err(RewriteError::unsupported_statement(
                "INSERT output policies spanning multiple aggregation CTEs are not supported",
            ));
        }
        let aggregate_select = cte_select(with, &cte_name).expect("aggregation CTE");
        for projection in projections {
            let key = render_expr(&projection.expr, None);
            if aggregate_aliases.contains_key(&key) {
                continue;
            }
            let base = projection.alias;
            let mut alias = base.clone();
            let mut suffix = 2;
            while !used_aliases.insert(alias.to_ascii_lowercase()) {
                alias = format!("{base}_{suffix}");
                suffix += 1;
            }
            let expr =
                map_aggregate_source_columns(projection.expr, with, aggregate_select, catalog)?;
            aggregate_aliases.insert(key, alias.clone());
            aggregate_projections.push((expr, alias));
        }
        policy_indices.insert(index);
        aggregation_cte = Some(cte_name.clone());
        policy_source_tables = Some(cte_transitive_source_tables(with, &cte_name));
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
        aggregate_projections,
    }))
}

fn policy_needs_insert_output_scope(
    store: &PolicyStore,
    index: usize,
    policy: &PolicyIr,
    constraint: &str,
    output_mapping: &OutputColumnMapping,
) -> Result<bool, RewriteError> {
    let expr = ConstraintExprCtx {
        store,
        index,
        stats: None,
    }
    .expr(constraint)?;
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
    let PolicyIr::Pgn {
        sink, sink_alias, ..
    } = policy;
    visit_expressions(expr, |expr| {
        if let Expr::CompoundIdentifier(parts) = expr
            && parts.len() == 2
            && (parts[0].value.eq_ignore_ascii_case(OUTPUT_MARKER)
                || sink
                    .iter()
                    .chain(sink_alias.iter())
                    .any(|name| parts[0].value.eq_ignore_ascii_case(name)))
            && output_mapping
                .expr_by_column
                .keys()
                .any(|column| column.eq_ignore_ascii_case(&parts[1].value))
        {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    })
    .is_break()
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
    let mut expr = ConstraintExprCtx {
        store,
        index,
        stats: None,
    }
    .expr(constraint)?;
    if let Some(PolicyIr::Pgn { source_aliases, .. }) = store.policy(index) {
        expr = super::columns::replace_source_alias_qualifiers(expr, source_aliases);
    }
    let mut projections = Vec::new();
    let mut seen = HashSet::new();
    let _: ControlFlow<()> = visit_expressions(&expr, |expr| {
        if let Expr::Function(function) = expr
            && registry.is_aggregate_call(function)
            && seen.insert(render_expr(expr, None))
        {
            let name = function.name.to_string().to_ascii_lowercase();
            let suffix = first_function_expr(function)
                .and_then(|arg| match arg {
                    Expr::CompoundIdentifier(parts) => parts.last().map(|id| id.value.clone()),
                    Expr::Identifier(id) => Some(id.value),
                    _ => None,
                })
                .unwrap_or_else(|| "value".to_string());
            projections.push(AggregateProjection {
                expr: expr.clone(),
                alias: passant_internal_name("__passant", &format!("{name}_{suffix}")),
            });
        }
        ControlFlow::Continue(())
    });
    Ok(projections)
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
) {
    // Aliases are allocated once across every deferred policy and existing projection.
    for (expr, alias) in &plan.aggregate_projections {
        select.projection.push(SelectItem::ExprWithAlias {
            expr: expr.clone(),
            alias: Ident::new(alias),
        });
    }
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

fn replace_aggregate_expr_recursive(mut expr: Expr, plan: &InsertOutputPolicyPlan) -> Expr {
    let _: ControlFlow<()> = visit_expressions_mut(&mut expr, |expr| {
        if let Some(alias) = plan.aggregate_aliases.get(&render_expr(expr, None)) {
            *expr = Expr::Identifier(Ident::new(alias));
        }
        ControlFlow::Continue(())
    });
    expr
}

// Track only direct column lineage. Computed expressions cannot safely stand in for
// the original source value, even if their output alias happens to match its name.
pub(crate) type InputColumn = (Expr, Option<(TableKey, String)>);

fn input_columns(
    with: &sqlparser::ast::With,
    select: &Select,
    catalog: &TableCatalog,
    visiting: &mut HashSet<String>,
) -> Vec<InputColumn> {
    let mut columns = Vec::new();
    for factor in select.from.iter().flat_map(|table| {
        std::iter::once(&table.relation).chain(table.joins.iter().map(|join| &join.relation))
    }) {
        let (name, alias, inner) = match factor {
            TableFactor::Table { name, alias, .. } => {
                let name = crate::identifiers::TableName::from_object_name(name).to_string();
                let inner = cte_select(with, &name);
                (name, alias.as_ref(), inner)
            }
            TableFactor::Derived {
                subquery,
                alias: Some(alias),
                ..
            } => {
                let SetExpr::Select(inner) = subquery.body.as_ref() else {
                    continue;
                };
                (alias.name.value.clone(), Some(alias), Some(inner.as_ref()))
            }
            _ => continue,
        };
        let qualifier = alias
            .map(|alias| alias.name.clone())
            .unwrap_or_else(|| Ident::new(&name));
        let outputs = if let Some(inner) = inner {
            if !visiting.insert(name.to_ascii_lowercase()) {
                continue;
            }
            let inputs = input_columns(with, inner, catalog, visiting);
            let mut outputs = Vec::new();
            for item in &inner.projection {
                match item {
                    SelectItem::Wildcard(_) => {
                        outputs.extend(inputs.iter().filter_map(|(expr, origin)| {
                            column_ident(expr).map(|id| (id.clone(), origin.clone()))
                        }))
                    }
                    SelectItem::QualifiedWildcard(table, _) => {
                        for (expr, origin) in &inputs {
                            if let Expr::CompoundIdentifier(parts) = expr
                                && parts[0].value.eq_ignore_ascii_case(&table.to_string())
                            {
                                outputs.push((parts.last().unwrap().clone(), origin.clone()));
                            }
                        }
                    }
                    _ => {
                        if let Some((expr, output)) = projection_expr_and_name(item) {
                            let id = match item {
                                SelectItem::ExprWithAlias { alias, .. } => Some(alias.clone()),
                                _ => column_ident(expr)
                                    .cloned()
                                    .or_else(|| output.map(Ident::new)),
                            };
                            if let Some(id) = id {
                                let matches = inputs
                                    .iter()
                                    .filter(|(candidate, _)| column_matches(expr, candidate))
                                    .collect::<Vec<_>>();
                                let origin = if matches.len() == 1 {
                                    matches[0].1.clone()
                                } else {
                                    None
                                };
                                outputs.push((id, origin));
                            }
                        }
                    }
                }
            }
            visiting.remove(&name.to_ascii_lowercase());
            outputs
        } else {
            catalog
                .columns(&name)
                .into_iter()
                .flatten()
                .map(|column| {
                    (
                        Ident::new(column),
                        Some((TableKey::new(&name), column.to_ascii_lowercase())),
                    )
                })
                .collect()
        };
        // A CTE column list renames its output before a table alias column list does.
        let cte_alias = with
            .cte_tables
            .iter()
            .find(|cte| cte.alias.name.value.eq_ignore_ascii_case(&name))
            .map(|cte| &cte.alias);
        for (index, (mut id, origin)) in outputs.into_iter().enumerate() {
            if let Some(column) = cte_alias.and_then(|alias| alias.columns.get(index)) {
                id = column.name.clone();
            }
            if let Some(column) = alias.and_then(|alias| alias.columns.get(index)) {
                id = column.name.clone();
            }
            columns.push((
                Expr::CompoundIdentifier(vec![qualifier.clone(), id]),
                origin,
            ));
        }
    }
    columns
}

fn column_ident(expr: &Expr) -> Option<&Ident> {
    match expr {
        Expr::Identifier(id) => Some(id),
        Expr::CompoundIdentifier(parts) => parts.last(),
        _ => None,
    }
}

fn column_matches(expr: &Expr, candidate: &Expr) -> bool {
    match (expr, candidate) {
        (Expr::Identifier(id), Expr::CompoundIdentifier(parts)) => parts
            .last()
            .is_some_and(|other| id.value.eq_ignore_ascii_case(&other.value)),
        (Expr::CompoundIdentifier(parts), Expr::CompoundIdentifier(other)) => {
            parts.len() == other.len()
                && parts
                    .iter()
                    .zip(other)
                    .all(|(a, b)| a.value.eq_ignore_ascii_case(&b.value))
        }
        _ => false,
    }
}

fn map_aggregate_source_columns(
    mut expr: Expr,
    with: &sqlparser::ast::With,
    select: &Select,
    catalog: &TableCatalog,
) -> Result<Expr, RewriteError> {
    let inputs = input_columns(with, select, catalog, &mut HashSet::new());
    let result = visit_expressions_mut(&mut expr, |expr| {
        if let Expr::CompoundIdentifier(parts) = expr {
            let Some(column) = crate::identifiers::QualifiedColumn::from_compound_identifier(parts)
            else {
                return ControlFlow::Continue(());
            };
            let key = (
                TableKey::from_table(&column.table),
                column.column.as_str().to_ascii_lowercase(),
            );
            let matches = inputs
                .iter()
                .filter(|(_, origin)| origin.as_ref() == Some(&key))
                .collect::<Vec<_>>();
            if matches.len() != 1 {
                return ControlFlow::Break(RewriteError::unsupported_statement(format!(
                    "cannot uniquely resolve deferred aggregate source column {expr} through upstream projections"
                )));
            }
            let replacement = &matches[0].0;
            let id = column_ident(replacement).expect("input column");
            *expr = if inputs
                .iter()
                .filter(|(input, _)| {
                    column_ident(input)
                        .is_some_and(|other| other.value.eq_ignore_ascii_case(&id.value))
                })
                .count()
                == 1
            {
                Expr::Identifier(id.clone())
            } else {
                replacement.clone()
            };
        }
        ControlFlow::Continue(())
    });
    match result {
        ControlFlow::Continue(()) => Ok(expr),
        ControlFlow::Break(error) => Err(error),
    }
}
