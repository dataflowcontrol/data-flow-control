//! Defer aggregate policy enforcement from derived scan inputs to parent HAVING.

use std::collections::{HashMap, HashSet};

use sqlparser::ast::{BinaryOperator, Expr, Ident, Select, SelectItem, SetExpr, TableFactor};

use crate::aggregate_registry::AggregateRegistry;
use crate::identifiers::TableKey;
use crate::policy::{PolicyIr, Resolution};
use crate::policy_store::{MultiSourceLookupMode, PolicyStore};
use crate::semiring::is_semiring_distributive_aggregate;
use crate::sql::{binary_comparison, function_call, passant_filter_temp_column, qualified_column};

use super::RewriteError;
use super::expr::{
    add_filter, first_function_expr, is_aggregate_name, parse_expr, projection_expr_and_name,
};
use super::policy_expr::{ConstraintExprCtx, non_distributive_aggregates};
use super::projection::{extract_policy_comparison_for_policy, select_is_aggregation};
use super::scope::TableScope;

#[derive(Debug, Clone)]
pub(crate) struct DerivedPolicyPropagation {
    pub parent_having_predicate: Expr,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct DerivedPolicyPrep {
    pub propagations: Vec<DerivedPolicyPropagation>,
    pub deferred_indices: HashSet<usize>,
}

struct HiddenProjection {
    expr: Expr,
    alias: String,
}

pub(crate) fn plan_derived_policy_propagations(
    store: &PolicyStore,
    parent_select: &Select,
    registry: &AggregateRegistry,
) -> Result<Option<DerivedPolicyPrep>, RewriteError> {
    if !select_is_aggregation(parent_select, registry) {
        return Ok(None);
    }

    let parent_scope = TableScope::from_select(parent_select);
    let mut prep = DerivedPolicyPrep::default();
    let mut hidden_by_policy: HashMap<usize, HiddenProjection> = HashMap::new();

    for table in &parent_select.from {
        collect_derived_propagations_from_factor(
            store,
            registry,
            &table.relation,
            &parent_scope.direct_base_tables,
            &mut prep,
            &mut hidden_by_policy,
        )?;
        for join in &table.joins {
            collect_derived_propagations_from_factor(
                store,
                registry,
                &join.relation,
                &parent_scope.direct_base_tables,
                &mut prep,
                &mut hidden_by_policy,
            )?;
        }
    }

    if prep.propagations.is_empty() {
        return Ok(None);
    }
    Ok(Some(prep))
}

pub(crate) fn apply_derived_hidden_projections(
    select: &mut Select,
    prep: &DerivedPolicyPrep,
    store: &PolicyStore,
    registry: &AggregateRegistry,
) -> Result<(), RewriteError> {
    let mut hidden_by_policy: HashMap<usize, HiddenProjection> = HashMap::new();
    let parent_scope = TableScope::from_select(select);

    for table in &mut select.from {
        inject_hidden_projections_in_factor(
            store,
            registry,
            &mut table.relation,
            &parent_scope.direct_base_tables,
            prep,
            &mut hidden_by_policy,
        )?;
        for join in &mut table.joins {
            inject_hidden_projections_in_factor(
                store,
                registry,
                &mut join.relation,
                &parent_scope.direct_base_tables,
                prep,
                &mut hidden_by_policy,
            )?;
        }
    }
    Ok(())
}

pub(crate) fn apply_derived_parent_having(
    select: &mut Select,
    prep: &DerivedPolicyPrep,
) -> Result<(), RewriteError> {
    for propagation in &prep.propagations {
        add_filter(select, propagation.parent_having_predicate.clone(), true)?;
    }
    Ok(())
}

fn collect_derived_propagations_from_factor(
    store: &PolicyStore,
    registry: &AggregateRegistry,
    factor: &TableFactor,
    parent_direct_tables: &HashSet<TableKey>,
    prep: &mut DerivedPolicyPrep,
    hidden_by_policy: &mut HashMap<usize, HiddenProjection>,
) -> Result<(), RewriteError> {
    let TableFactor::Derived {
        subquery,
        alias: Some(alias),
        ..
    } = factor
    else {
        return Ok(());
    };

    if alias.name.value.eq_ignore_ascii_case("exists_subquery")
        || alias.name.value.eq_ignore_ascii_case("in_subquery")
    {
        return Ok(());
    }

    let SetExpr::Select(inner_select) = subquery.body.as_ref() else {
        return Ok(());
    };

    let derived_alias = alias.name.value.clone();
    let inner_scope = TableScope::from_select(inner_select);
    let inner_is_aggregation = select_is_aggregation(inner_select, registry);

    for index in store
        .candidate_scope_lookup(
            &inner_scope.direct_base_tables,
            None,
            MultiSourceLookupMode::Subset,
        )
        .iter()
    {
        if prep.deferred_indices.contains(&index) {
            continue;
        }
        let Some(policy) = store.policy(index) else {
            continue;
        };
        let PolicyIr::Pgn {
            sources,
            constraint,
            on_fail: Resolution::Remove,
            required_sources,
            sink,
            ..
        } = policy
        else {
            continue;
        };
        if !required_sources.is_empty() || sink.is_some() {
            continue;
        }
        if !sources.iter().all(|source| {
            inner_scope
                .direct_base_tables
                .contains(&TableKey::new(source))
        }) {
            continue;
        }
        if sources
            .iter()
            .any(|source| parent_direct_tables.contains(&TableKey::new(source)))
        {
            continue;
        }

        let constraint_ctx = ConstraintExprCtx {
            store,
            index,
            stats: None,
        };
        let constraint_expr = constraint_ctx.expr(constraint)?;
        if !non_distributive_aggregates(&constraint_expr, registry)?.is_empty() {
            continue;
        }

        if inner_is_aggregation && !inner_derived_reaggregation_allowed(&constraint_expr, registry)?
        {
            continue;
        }

        let hidden = hidden_by_policy.entry(index).or_insert_with(|| {
            build_hidden_projection(store, registry, index, constraint, inner_select)
                .expect("hidden projection should succeed for validated policy")
        });

        let parent_predicate = build_parent_having_predicate(
            store,
            index,
            constraint,
            &derived_alias,
            &hidden.alias,
            &constraint_expr,
        )?;

        prep.deferred_indices.insert(index);
        prep.propagations.push(DerivedPolicyPropagation {
            parent_having_predicate: parent_predicate,
        });
    }

    Ok(())
}

fn inject_hidden_projections_in_factor(
    store: &PolicyStore,
    registry: &AggregateRegistry,
    factor: &mut TableFactor,
    parent_direct_tables: &HashSet<TableKey>,
    prep: &DerivedPolicyPrep,
    hidden_by_policy: &mut HashMap<usize, HiddenProjection>,
) -> Result<(), RewriteError> {
    let TableFactor::Derived {
        subquery,
        alias: Some(alias),
        ..
    } = factor
    else {
        return Ok(());
    };

    if alias.name.value.eq_ignore_ascii_case("exists_subquery")
        || alias.name.value.eq_ignore_ascii_case("in_subquery")
    {
        return Ok(());
    }

    let SetExpr::Select(inner_select) = subquery.body.as_mut() else {
        return Ok(());
    };

    let inner_scope = TableScope::from_select(inner_select);

    for index in prep.deferred_indices.iter().copied() {
        let Some(policy) = store.policy(index) else {
            continue;
        };
        let PolicyIr::Pgn {
            sources,
            constraint,
            ..
        } = policy;
        if !sources.iter().all(|source| {
            inner_scope
                .direct_base_tables
                .contains(&TableKey::new(source))
        }) || sources
            .iter()
            .any(|source| parent_direct_tables.contains(&TableKey::new(source)))
        {
            continue;
        }

        let hidden = hidden_by_policy.entry(index).or_insert_with(|| {
            build_hidden_projection(store, registry, index, constraint, inner_select)
                .expect("hidden projection should succeed for deferred policy")
        });

        if projection_has_equivalent(inner_select, &hidden.expr, &hidden.alias) {
            continue;
        }

        inner_select.projection.push(SelectItem::ExprWithAlias {
            expr: hidden.expr.clone(),
            alias: Ident::new(&hidden.alias),
        });
    }

    Ok(())
}

fn inner_derived_reaggregation_allowed(
    constraint_expr: &Expr,
    registry: &AggregateRegistry,
) -> Result<bool, RewriteError> {
    let aggregates = non_distributive_aggregates(constraint_expr, registry)?;
    if !aggregates.is_empty() {
        return Ok(false);
    }
    Ok(policy_top_level_aggregate_is_distributive(constraint_expr))
}

fn policy_top_level_aggregate_is_distributive(expr: &Expr) -> bool {
    let Expr::BinaryOp { left, .. } = expr else {
        return false;
    };
    let Expr::Function(function) = left.as_ref() else {
        return false;
    };
    is_semiring_distributive_aggregate(&function.name.to_string())
}

fn build_hidden_projection(
    store: &PolicyStore,
    registry: &AggregateRegistry,
    policy_index: usize,
    constraint: &str,
    inner_select: &Select,
) -> Result<HiddenProjection, RewriteError> {
    let constraint_ctx = ConstraintExprCtx {
        store,
        index: policy_index,
        stats: None,
    };
    let constraint_expr = constraint_ctx.expr(constraint)?;
    let (metric_expr, _agg_name) = policy_metric_from_constraint(&constraint_expr, registry)?;

    let alias = find_equivalent_projection_alias(inner_select, &metric_expr).unwrap_or_else(|| {
        passant_filter_temp_column(&format!(
            "policy_{}_{}",
            policy_index,
            alias_suffix_from_expr(&metric_expr)
        ))
    });

    Ok(HiddenProjection {
        expr: metric_expr,
        alias,
    })
}

fn policy_metric_from_constraint(
    constraint_expr: &Expr,
    registry: &AggregateRegistry,
) -> Result<(Expr, String), RewriteError> {
    let Expr::BinaryOp { left, .. } = constraint_expr else {
        return Err(RewriteError::unsupported_statement(
            "derived policy propagation requires a comparison constraint",
        ));
    };
    let Expr::Function(function) = left.as_ref() else {
        return Err(RewriteError::unsupported_statement(
            "derived policy propagation requires an aggregate constraint",
        ));
    };
    if !is_aggregate_name(&function.name.to_string(), registry) {
        return Err(RewriteError::unsupported_statement(
            "derived policy propagation requires an aggregate constraint",
        ));
    }
    let arg = first_function_expr(function).ok_or_else(|| {
        RewriteError::unsupported_statement("policy aggregate is missing an argument")
    })?;
    Ok((arg, function.name.to_string()))
}

fn build_parent_having_predicate(
    store: &PolicyStore,
    policy_index: usize,
    constraint: &str,
    derived_alias: &str,
    hidden_alias: &str,
    constraint_expr: &Expr,
) -> Result<Expr, RewriteError> {
    let (_, threshold, op) =
        extract_policy_comparison_for_policy(store, policy_index, constraint, None)?;
    let Expr::BinaryOp { left, .. } = constraint_expr else {
        return Err(RewriteError::unsupported_statement(
            "derived policy propagation requires a comparison constraint",
        ));
    };
    let Expr::Function(function) = left.as_ref() else {
        return Err(RewriteError::unsupported_statement(
            "derived policy propagation requires an aggregate constraint",
        ));
    };
    let agg_name = function.name.to_string();
    let metric = qualified_column(derived_alias, hidden_alias);
    let parent_metric = function_call(&agg_name, vec![metric]);
    let rhs = parse_expr(&threshold)?;
    Ok(binary_comparison(parent_metric, op_from_str(op), rhs))
}

fn op_from_str(op: &str) -> BinaryOperator {
    match op {
        ">" => BinaryOperator::Gt,
        ">=" => BinaryOperator::GtEq,
        "<" => BinaryOperator::Lt,
        "<=" => BinaryOperator::LtEq,
        "=" => BinaryOperator::Eq,
        "!=" | "<>" => BinaryOperator::NotEq,
        _ => BinaryOperator::GtEq,
    }
}

fn alias_suffix_from_expr(expr: &Expr) -> String {
    crate::sql::render_expr(expr, None)
        .replace('.', "_")
        .replace(['(', ')', ' ', '*', '-', '+', '/'], "_")
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .collect::<String>()
}

fn projection_has_equivalent(select: &Select, expr: &Expr, alias: &str) -> bool {
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

fn find_equivalent_projection_alias(select: &Select, metric_expr: &Expr) -> Option<String> {
    let key = crate::sql::ExprKey::from_expr(metric_expr);
    for item in &select.projection {
        let Some((expr, alias)) = projection_expr_and_name(item) else {
            continue;
        };
        if crate::sql::ExprKey::from_expr(expr) != key {
            continue;
        }
        if let Some(name) = alias {
            return Some(name.to_string());
        }
        if let Some(name) = super::expr::projected_column_name(expr) {
            return Some(name);
        }
    }
    None
}
