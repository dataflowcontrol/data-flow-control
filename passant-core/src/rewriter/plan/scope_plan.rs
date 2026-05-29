use std::collections::{HashMap, HashSet};

use sqlparser::ast::{Expr, JoinConstraint, JoinOperator, Select, TableWithJoins};

use crate::catalog::TableCatalog;
use crate::identifiers::TableKey;
use crate::policy::{PolicyIr, Resolution};
use crate::policy_store::PolicyStore;
use crate::query_analysis::SelectAnalysis;
use crate::rewrite_stats::RewriteStatsCell;
use crate::rewriter::aggregates::transform_scan_aggregates;
use crate::rewriter::columns::rewrite_column_qualifiers;
use crate::rewriter::expr::{
    add_filter, and_expr, filter_table_factor, join_conjuncts, table_factor_base_and_alias,
};
use crate::rewriter::helpers::{table_joins_all_inner, table_with_joins_base_tables};
use crate::rewriter::policy_expr::{
    ConstraintExprCtx, join_pushdown_expr, non_distributive_aggregates,
};
use crate::rewriter::types::RewriteContext;
use crate::source_sets::{select_has_anti_join, select_has_full_join};

use super::actions::{PolicyResolutionAction, apply_policy_resolution_actions};
use super::applicability::resolve_scope_policies;

/// Planned filter on a FROM-clause table factor (relation or joined table).
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum TableFactorFilterTarget {
    FromRelation {
        from_index: usize,
    },
    JoinRelation {
        from_index: usize,
        join_index: usize,
    },
}

#[derive(Debug, Clone)]
pub struct TableFactorFilterAction {
    pub target: TableFactorFilterTarget,
    pub expr: Expr,
}

#[derive(Debug, Clone)]
pub struct JoinOnFilterAction {
    pub from_index: usize,
    pub join_index: usize,
    pub expr: Expr,
}

#[derive(Debug, Clone)]
pub struct MultiSourceJoinPushAction {
    pub from_index: usize,
    pub policy_index: usize,
    pub expr: Expr,
}

/// Immutable rewrite plan for one SELECT scope (Full-Push).
#[derive(Debug, Clone, Default)]
pub struct SelectRewritePlan {
    pub diagnostics: super::ScopePlanDiagnostics,
    pub table_factor_filters: Vec<TableFactorFilterAction>,
    pub join_on_filters: Vec<JoinOnFilterAction>,
    pub selection_filters: Vec<Expr>,
    pub multi_source_join_pushes: Vec<MultiSourceJoinPushAction>,
    pub fully_pushed_indices: HashSet<usize>,
    pub(crate) policy_actions: Vec<PolicyResolutionAction>,
}

pub(crate) fn plan_select_rewrite(
    store: &PolicyStore,
    catalog: &TableCatalog,
    stats: Option<&RewriteStatsCell>,
    select: &Select,
    analysis: &SelectAnalysis,
    context: &RewriteContext,
    exists_handled: &HashSet<usize>,
) -> Result<SelectRewritePlan, crate::diagnostics::RewriteError> {
    let mut plan = SelectRewritePlan::default();

    plan_join_input_source_filters(store, catalog, stats, select, analysis, &mut plan)?;
    plan_join_policy_pushdown(store, catalog, stats, select, analysis, &mut plan)?;

    let (applicable, policy_diag) = resolve_scope_policies(
        store,
        &analysis.scope.direct_base_tables,
        context.sink.as_deref(),
        context.allow_partial_source_visibility,
        &plan.fully_pushed_indices,
        exists_handled,
    );
    plan.diagnostics.candidate_policies = policy_diag.candidate_policies;
    plan.diagnostics.applicable_policies = policy_diag.applicable_policies;
    plan.diagnostics.dominated_policies = policy_diag.dominated_policies;
    plan.diagnostics.skipped_pushdown = policy_diag.skipped_pushdown;
    plan.diagnostics.skipped_exists_handled = policy_diag.skipped_exists_handled;

    if applicable.is_empty() {
        return Ok(plan);
    }

    plan.policy_actions = super::actions::build_policy_resolution_actions(
        store,
        catalog,
        stats,
        &analysis.scope,
        context,
        analysis.is_aggregation,
        applicable,
    )?;
    plan.diagnostics.emitted_policy_actions = plan.policy_actions.len();
    Ok(plan)
}

fn plan_join_input_source_filters(
    store: &PolicyStore,
    catalog: &TableCatalog,
    stats: Option<&RewriteStatsCell>,
    select: &Select,
    analysis: &SelectAnalysis,
    plan: &mut SelectRewritePlan,
) -> Result<(), crate::diagnostics::RewriteError> {
    if (!select_has_full_join(select) && !select_has_anti_join(select)) || store.is_empty() {
        return Ok(());
    }

    let occurrence_counts = &analysis.source_occurrence_counts;
    let mut pushed_counts: HashMap<usize, usize> = HashMap::new();
    for (from_index, table) in select.from.iter().enumerate() {
        let mut relation_filters = Vec::new();
        if table.joins.iter().any(|join| {
            matches!(
                join.join_operator,
                JoinOperator::FullOuter(_) | JoinOperator::RightAnti(_)
            )
        }) && let Some((base, _)) = table_factor_base_and_alias(&table.relation)
        {
            for index in store.join_pushdown_candidates(&TableKey::new(&base)) {
                let Some(policy) = store.policy(index) else {
                    continue;
                };
                let constraint_ctx = ConstraintExprCtx {
                    store,
                    index,
                    stats,
                };
                relation_filters.push((
                    TableFactorFilterTarget::FromRelation { from_index },
                    join_pushdown_expr(policy, &constraint_ctx, &base, None, catalog)?,
                ));
                *pushed_counts.entry(index).or_default() += 1;
            }
        }
        for (join_index, join) in table.joins.iter().enumerate() {
            if matches!(
                join.join_operator,
                JoinOperator::FullOuter(_) | JoinOperator::Anti(_) | JoinOperator::LeftAnti(_)
            ) && let Some((base, _)) = table_factor_base_and_alias(&join.relation)
            {
                for index in store.join_pushdown_candidates(&TableKey::new(&base)) {
                    let Some(policy) = store.policy(index) else {
                        continue;
                    };
                    let constraint_ctx = ConstraintExprCtx {
                        store,
                        index,
                        stats,
                    };
                    relation_filters.push((
                        TableFactorFilterTarget::JoinRelation {
                            from_index,
                            join_index,
                        },
                        join_pushdown_expr(policy, &constraint_ctx, &base, None, catalog)?,
                    ));
                    *pushed_counts.entry(index).or_default() += 1;
                }
            }
        }
        for (target, expr) in relation_filters {
            plan.table_factor_filters
                .push(TableFactorFilterAction { target, expr });
        }
    }

    record_fully_pushed(
        store,
        occurrence_counts,
        pushed_counts,
        &mut plan.fully_pushed_indices,
    );
    Ok(())
}

fn plan_join_policy_pushdown(
    store: &PolicyStore,
    catalog: &TableCatalog,
    stats: Option<&RewriteStatsCell>,
    select: &Select,
    analysis: &SelectAnalysis,
    plan: &mut SelectRewritePlan,
) -> Result<(), crate::diagnostics::RewriteError> {
    let occurrence_counts = &analysis.source_occurrence_counts;
    let mut pushed_counts: HashMap<usize, usize> = HashMap::new();
    let table_scope = &analysis.scope;

    for (from_index, table) in select.from.iter().enumerate() {
        let left_base_and_alias = table_factor_base_and_alias(&table.relation);
        if table_joins_all_inner(table)
            && let Some((base, alias)) = &left_base_and_alias
        {
            for index in store.join_pushdown_candidates(&TableKey::new(base)) {
                let Some(policy) = store.policy(index) else {
                    continue;
                };
                let constraint_ctx = ConstraintExprCtx {
                    store,
                    index,
                    stats,
                };
                plan.selection_filters.push(join_pushdown_expr(
                    policy,
                    &constraint_ctx,
                    base,
                    alias.clone(),
                    catalog,
                )?);
                *pushed_counts.entry(index).or_default() += 1;
            }
        }
        for (join_index, _join) in table.joins.iter().enumerate() {
            let Some((base, alias)) =
                join_pushdown_on_target(table, join_index, &left_base_and_alias)
            else {
                continue;
            };
            for index in store.join_pushdown_candidates(&TableKey::new(&base)) {
                let Some(policy) = store.policy(index) else {
                    continue;
                };
                let constraint_ctx = ConstraintExprCtx {
                    store,
                    index,
                    stats,
                };
                plan.join_on_filters.push(JoinOnFilterAction {
                    from_index,
                    join_index,
                    expr: join_pushdown_expr(
                        policy,
                        &constraint_ctx,
                        &base,
                        alias.clone(),
                        catalog,
                    )?,
                });
                *pushed_counts.entry(index).or_default() += 1;
            }
        }
    }

    let candidate_ids = store.candidate_ids_for_tables(&analysis.scope.direct_base_tables);
    for index in candidate_ids {
        if plan.fully_pushed_indices.contains(&index) {
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
        if !required_sources.is_empty() || sink.is_some() || sources.len() < 2 {
            continue;
        }
        let constraint_ctx = ConstraintExprCtx {
            store,
            index,
            stats,
        };
        let expr = constraint_ctx.expr(constraint)?;
        if !non_distributive_aggregates(&expr)?.is_empty() {
            continue;
        }
        for (from_index, table) in select.from.iter().enumerate() {
            if !table_joins_all_inner(table) {
                continue;
            }
            let bases = table_with_joins_base_tables(table);
            if !sources
                .iter()
                .all(|source| bases.contains(&TableKey::new(source)))
            {
                continue;
            }
            let mut transformed = transform_scan_aggregates(expr.clone())?;
            rewrite_column_qualifiers(&mut transformed, &table_scope.alias_by_base);
            plan.multi_source_join_pushes
                .push(MultiSourceJoinPushAction {
                    from_index,
                    policy_index: index,
                    expr: transformed,
                });
            plan.fully_pushed_indices.insert(index);
            break;
        }
    }

    record_fully_pushed(
        store,
        occurrence_counts,
        pushed_counts,
        &mut plan.fully_pushed_indices,
    );
    Ok(())
}

fn join_pushdown_on_target(
    table: &TableWithJoins,
    join_index: usize,
    left_base_and_alias: &Option<(String, Option<String>)>,
) -> Option<(String, Option<String>)> {
    let join = table.joins.get(join_index)?;
    match &join.join_operator {
        JoinOperator::Inner(JoinConstraint::On(_))
        | JoinOperator::LeftOuter(JoinConstraint::On(_))
        | JoinOperator::Semi(JoinConstraint::On(_))
        | JoinOperator::LeftSemi(JoinConstraint::On(_)) => {
            table_factor_base_and_alias(&join.relation)
        }
        JoinOperator::RightOuter(JoinConstraint::On(_))
        | JoinOperator::RightSemi(JoinConstraint::On(_)) => left_base_and_alias.clone(),
        JoinOperator::FullOuter(_) => None,
        _ => None,
    }
}

fn record_fully_pushed(
    store: &PolicyStore,
    occurrence_counts: &HashMap<String, usize>,
    pushed_counts: HashMap<usize, usize>,
    fully_pushed: &mut HashSet<usize>,
) {
    for (index, count) in pushed_counts {
        let Some(policy) = store.policy(index) else {
            continue;
        };
        let Some(source) = policy.sources().first() else {
            continue;
        };
        if occurrence_counts
            .get(&source.to_ascii_lowercase())
            .is_some_and(|occurrences| count >= *occurrences)
        {
            fully_pushed.insert(index);
        }
    }
}

pub(crate) fn apply_select_rewrite_plan(
    select: &mut Select,
    plan: SelectRewritePlan,
    is_aggregation: bool,
) -> Result<(), crate::diagnostics::RewriteError> {
    let mut grouped_table_filters: HashMap<TableFactorFilterTarget, Vec<Expr>> = HashMap::new();
    for action in plan.table_factor_filters {
        grouped_table_filters
            .entry(action.target)
            .or_default()
            .push(action.expr);
    }
    for (target, exprs) in grouped_table_filters {
        let filter = join_conjuncts(exprs);
        match target {
            TableFactorFilterTarget::FromRelation { from_index } => {
                if let Some(table) = select.from.get_mut(from_index) {
                    filter_table_factor(&mut table.relation, filter)?;
                }
            }
            TableFactorFilterTarget::JoinRelation {
                from_index,
                join_index,
            } => {
                if let Some(table) = select.from.get_mut(from_index)
                    && let Some(join) = table.joins.get_mut(join_index)
                {
                    filter_table_factor(&mut join.relation, filter)?;
                }
            }
        }
    }

    for action in plan.join_on_filters {
        if let Some(table) = select.from.get_mut(action.from_index)
            && let Some(join) = table.joins.get_mut(action.join_index)
            && let JoinOperator::Inner(JoinConstraint::On(existing_on))
            | JoinOperator::LeftOuter(JoinConstraint::On(existing_on))
            | JoinOperator::RightOuter(JoinConstraint::On(existing_on))
            | JoinOperator::Semi(JoinConstraint::On(existing_on))
            | JoinOperator::LeftSemi(JoinConstraint::On(existing_on))
            | JoinOperator::RightSemi(JoinConstraint::On(existing_on)) = &mut join.join_operator
        {
            *existing_on = and_expr(existing_on.clone(), action.expr);
        }
    }

    for action in plan.multi_source_join_pushes {
        if let Some(table) = select.from.get_mut(action.from_index)
            && let Some(join) = table.joins.last_mut()
            && let JoinOperator::Inner(JoinConstraint::On(existing_on)) = &mut join.join_operator
        {
            *existing_on = and_expr(existing_on.clone(), action.expr);
        }
    }

    for expr in plan.selection_filters {
        add_filter(select, expr, false)?;
    }

    apply_policy_resolution_actions(select, &plan.policy_actions, is_aggregation)?;

    Ok(())
}
