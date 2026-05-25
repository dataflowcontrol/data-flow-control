//! Rewrite planning: candidate lookup, dominance, and action emission without AST mutation.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use sqlparser::ast::{Expr, JoinConstraint, JoinOperator, Select, Statement, TableWithJoins};

use super::aggregates::transform_scan_aggregates;
use super::columns::{replace_sink_columns, rewrite_column_qualifiers};
use super::expr::{
    add_filter, and_expr, apply_resolution, bool_literal, filter_table_factor, join_conjuncts,
    table_factor_base_and_alias,
};
use super::helpers::{
    prune_dominated_applicable_with_store, table_joins_all_inner, table_with_joins_base_tables,
};
use super::policy_expr::{
    ConstraintExprCtx, build_compat_dfc_filter_expr, build_invalidate_projection_expr,
    build_pgn_over_filter_expr, compiled_policy_applicability, join_pushdown_expr,
    non_distributive_aggregates, scan_policy_expr, unique_column_guard_from_constraint,
};
use super::scope::TableScope;
use super::types::{PolicyApplicability, RewriteContext};
use crate::catalog::TableCatalog;
use crate::diagnostics::RewriteError;
use crate::identifiers::{AliasByBase, TableKey};
use crate::policy::{PgnPolicyKind, PolicyIr, Resolution};
use crate::policy_store::{MultiSourceLookupMode, PolicyStore};
use crate::query_analysis::{SelectAnalysis, StatementAnalysis};
use crate::rewrite_stats::RewriteStatsCell;
use crate::source_sets::{select_has_anti_join, select_has_full_join};

/// Aggregated planning diagnostics across all SELECT scopes in one statement rewrite.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatementRewriteSummary {
    pub scope_diagnostics: Vec<ScopePlanDiagnostics>,
}

impl StatementRewriteSummary {
    pub fn push_scope(&mut self, diagnostics: ScopePlanDiagnostics) {
        self.scope_diagnostics.push(diagnostics);
    }

    pub fn aggregate(&self) -> ScopePlanDiagnostics {
        self.scope_diagnostics
            .iter()
            .fold(ScopePlanDiagnostics::default(), |mut acc, scope| {
                acc.candidate_policies += scope.candidate_policies;
                acc.applicable_policies += scope.applicable_policies;
                acc.dominated_policies += scope.dominated_policies;
                acc.skipped_pushdown += scope.skipped_pushdown;
                acc.skipped_exists_handled += scope.skipped_exists_handled;
                acc.emitted_policy_actions += scope.emitted_policy_actions;
                acc
            })
    }
}

#[derive(Debug, Default)]
pub(crate) struct StatementRewriteSummaryCell {
    inner: Mutex<StatementRewriteSummary>,
}

impl StatementRewriteSummaryCell {
    pub fn reset(&self) {
        if let Ok(mut summary) = self.inner.lock() {
            summary.scope_diagnostics.clear();
        }
    }

    pub fn record_scope(&self, diagnostics: ScopePlanDiagnostics) {
        if let Ok(mut summary) = self.inner.lock() {
            summary.push_scope(diagnostics);
        }
    }

    pub fn snapshot(&self) -> StatementRewriteSummary {
        self.inner
            .lock()
            .map(|summary| summary.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod summary_tests {
    use super::*;

    #[test]
    fn statement_rewrite_summary_aggregates_scope_diagnostics() {
        let mut summary = StatementRewriteSummary::default();
        summary.push_scope(ScopePlanDiagnostics {
            candidate_policies: 2,
            applicable_policies: 1,
            dominated_policies: 1,
            ..ScopePlanDiagnostics::default()
        });
        summary.push_scope(ScopePlanDiagnostics {
            candidate_policies: 3,
            applicable_policies: 2,
            dominated_policies: 0,
            emitted_policy_actions: 2,
            ..ScopePlanDiagnostics::default()
        });
        let total = summary.aggregate();
        assert_eq!(total.candidate_policies, 5);
        assert_eq!(total.applicable_policies, 3);
        assert_eq!(total.dominated_policies, 1);
        assert_eq!(total.emitted_policy_actions, 2);
    }
}

/// Counts produced while planning policy actions for one SELECT scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScopePlanDiagnostics {
    pub candidate_policies: usize,
    pub applicable_policies: usize,
    pub dominated_policies: usize,
    pub skipped_pushdown: usize,
    pub skipped_exists_handled: usize,
    pub emitted_policy_actions: usize,
}

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

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum PolicyResolutionAction {
    CompatDfc {
        filter: Expr,
        projection: Option<Expr>,
        on_fail: Resolution,
        description: Option<String>,
    },
    PgnOver {
        filter: Expr,
        on_fail: Resolution,
        description: Option<String>,
    },
}

/// Immutable rewrite plan for one SELECT scope (Full-Push).
#[derive(Debug, Clone, Default)]
pub struct SelectRewritePlan {
    pub diagnostics: ScopePlanDiagnostics,
    pub table_factor_filters: Vec<TableFactorFilterAction>,
    pub join_on_filters: Vec<JoinOnFilterAction>,
    pub selection_filters: Vec<Expr>,
    pub multi_source_join_pushes: Vec<MultiSourceJoinPushAction>,
    pub fully_pushed_indices: HashSet<usize>,
    pub(crate) policy_actions: Vec<PolicyResolutionAction>,
    pub apply_aggregate_scan_columns: bool,
}

/// Resolve indexed candidates, applicability, and dominance for a scope.
pub(crate) fn resolve_scope_policies<'a>(
    store: &'a PolicyStore,
    direct_tables: &HashSet<TableKey>,
    sink: Option<&str>,
    allow_partial_source_visibility: bool,
    exclude_pushdown: &HashSet<usize>,
    exclude_exists: &HashSet<usize>,
) -> (
    Vec<(usize, &'a PolicyIr, PolicyApplicability)>,
    ScopePlanDiagnostics,
) {
    let sink_key = sink.map(TableKey::new);
    let multi_source_mode = if allow_partial_source_visibility {
        MultiSourceLookupMode::AnyOverlap
    } else {
        MultiSourceLookupMode::Subset
    };
    store.debug_assert_candidates_match_slow_scan(
        direct_tables,
        sink_key.as_ref(),
        multi_source_mode,
    );
    let mut candidate_count = 0usize;
    let mut skipped_pushdown = 0usize;
    let mut skipped_exists = 0usize;
    let indexed_applicable = store
        .candidate_scope_lookup(direct_tables, sink_key.as_ref(), multi_source_mode)
        .iter()
        .filter(|index| {
            candidate_count += 1;
            if exclude_pushdown.contains(index) {
                skipped_pushdown += 1;
                return false;
            }
            if exclude_exists.contains(index) {
                skipped_exists += 1;
                return false;
            }
            true
        })
        .filter_map(|index| {
            let compiled = store.compiled(index)?;
            compiled_policy_applicability(
                compiled,
                direct_tables,
                sink_key.as_ref(),
                allow_partial_source_visibility,
            )
            .map(|applicability| (index, &compiled.policy, applicability))
        })
        .collect::<Vec<_>>();
    let mut diagnostics = ScopePlanDiagnostics {
        candidate_policies: candidate_count,
        ..ScopePlanDiagnostics::default()
    };
    diagnostics.skipped_pushdown = skipped_pushdown;
    diagnostics.skipped_exists_handled = skipped_exists;
    diagnostics.applicable_policies = indexed_applicable.len();
    if indexed_applicable.is_empty() {
        return (Vec::new(), diagnostics);
    }
    let (applicable, dominated) = prune_dominated_applicable_with_store(store, indexed_applicable);
    diagnostics.dominated_policies = dominated;
    (applicable, diagnostics)
}

/// Plan candidate/applicable counts for every SELECT scope in a statement without mutating SQL.
pub fn plan_statement_rewrite_summary(
    store: &PolicyStore,
    statement: &Statement,
) -> StatementRewriteSummary {
    let analysis = StatementAnalysis::from_statement(statement);
    let sink = analysis.sink.as_ref().map(|key| key.as_str());
    let mut summary = StatementRewriteSummary::default();
    for select_analysis in analysis.select_scopes {
        let (_, diagnostics) = resolve_scope_policies(
            store,
            &select_analysis.scope.direct_base_tables,
            sink,
            false,
            &HashSet::new(),
            &HashSet::new(),
        );
        summary.push_scope(diagnostics);
    }
    summary
}

fn build_policy_resolution_actions(
    store: &PolicyStore,
    catalog: &TableCatalog,
    stats: Option<&RewriteStatsCell>,
    table_scope: &TableScope,
    context: &RewriteContext,
    is_aggregation: bool,
    applicable: Vec<(usize, &PolicyIr, PolicyApplicability)>,
) -> Result<Vec<PolicyResolutionAction>, RewriteError> {
    let mut actions = Vec::new();
    for (index, policy, applicability) in applicable {
        let constraint_ctx = ConstraintExprCtx {
            store,
            index,
            stats,
        };
        match policy {
            PolicyIr::CompatDfc {
                sources,
                constraint,
                on_fail,
                sink_alias,
                description,
                ..
            } => {
                let mut expr = build_compat_dfc_filter_expr(
                    sources,
                    constraint,
                    sink_alias,
                    applicability,
                    context,
                    table_scope,
                    is_aggregation,
                    &constraint_ctx,
                )?;
                if let Some(guard) =
                    unique_column_guard_from_constraint(constraint, catalog, &constraint_ctx)
                {
                    expr = and_expr(guard, expr);
                }
                let projection = if matches!(on_fail, Resolution::Invalidate)
                    && context.sink.is_some()
                    && sources.len() > 1
                {
                    Some(build_invalidate_projection_expr(
                        sources,
                        constraint,
                        sink_alias,
                        applicability,
                        context,
                        table_scope,
                        &constraint_ctx,
                    )?)
                } else {
                    Some(expr.clone())
                };
                actions.push(PolicyResolutionAction::CompatDfc {
                    filter: expr,
                    projection,
                    on_fail: *on_fail,
                    description: description.clone(),
                });
            }
            PolicyIr::CompatAggregate(_) => {}
            PolicyIr::NativePgn(pgn) if pgn.kind == PgnPolicyKind::Over => {
                let expr = build_pgn_over_filter_expr(
                    &pgn.scope.sources,
                    &pgn.constraint,
                    &pgn.scope.sink_alias,
                    applicability,
                    context,
                    table_scope,
                    &constraint_ctx,
                )?;
                actions.push(PolicyResolutionAction::PgnOver {
                    filter: expr,
                    on_fail: pgn.on_fail,
                    description: pgn.description.clone(),
                });
            }
            PolicyIr::NativePgn(_) => {}
        }
    }
    Ok(actions)
}

/// Build policy filter actions for a scope (shared by Full-Push, Partial-Push, HAVING).
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_policy_filter_actions(
    store: &PolicyStore,
    catalog: &TableCatalog,
    stats: Option<&RewriteStatsCell>,
    direct_tables: &HashSet<TableKey>,
    table_scope: &TableScope,
    sink: Option<&str>,
    context: &RewriteContext,
    is_aggregation: bool,
    exclude_pushdown: &HashSet<usize>,
    exclude_exists: &HashSet<usize>,
) -> Result<(Vec<PolicyResolutionAction>, ScopePlanDiagnostics), RewriteError> {
    let (applicable, diagnostics) = resolve_scope_policies(
        store,
        direct_tables,
        sink,
        context.allow_partial_source_visibility,
        exclude_pushdown,
        exclude_exists,
    );
    if applicable.is_empty() {
        return Ok((Vec::new(), diagnostics));
    }
    let actions = build_policy_resolution_actions(
        store,
        catalog,
        stats,
        table_scope,
        context,
        is_aggregation,
        applicable,
    )?;
    Ok((actions, diagnostics))
}

pub(crate) fn apply_policy_resolution_actions(
    select: &mut Select,
    actions: &[PolicyResolutionAction],
    is_aggregation: bool,
) -> Result<(), RewriteError> {
    for action in actions {
        match action {
            PolicyResolutionAction::CompatDfc {
                filter,
                projection,
                on_fail,
                description,
            } => {
                apply_resolution(
                    select,
                    filter.clone(),
                    *on_fail,
                    description.as_deref(),
                    is_aggregation,
                    projection.clone(),
                )?;
            }
            PolicyResolutionAction::PgnOver {
                filter,
                on_fail,
                description,
            } => {
                apply_resolution(
                    select,
                    filter.clone(),
                    *on_fail,
                    description.as_deref(),
                    is_aggregation,
                    None,
                )?;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub enum UpdatePolicyAction {
    CompatDfc {
        filter: Expr,
        on_fail: Resolution,
        description: Option<String>,
    },
    PgnUpdate {
        filter: Expr,
        on_fail: Resolution,
        description: Option<String>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct UpdateScopePlan {
    pub diagnostics: ScopePlanDiagnostics,
    pub actions: Vec<UpdatePolicyAction>,
}

pub(crate) fn plan_update_scope(
    store: &PolicyStore,
    _catalog: &TableCatalog,
    stats: Option<&RewriteStatsCell>,
    table_scope: &TableScope,
    sink: Option<&str>,
    context: &RewriteContext,
) -> Result<UpdateScopePlan, RewriteError> {
    let (applicable, diagnostics) = resolve_scope_policies(
        store,
        &table_scope.base_tables,
        sink,
        false,
        &HashSet::new(),
        &HashSet::new(),
    );
    let mut plan = UpdateScopePlan {
        diagnostics,
        ..UpdateScopePlan::default()
    };
    if applicable.is_empty() {
        return Ok(plan);
    }
    for (index, policy, applicability) in applicable {
        let constraint_ctx = ConstraintExprCtx {
            store,
            index,
            stats,
        };
        match policy {
            PolicyIr::CompatDfc {
                constraint,
                on_fail,
                sink_alias,
                description,
                ..
            } => {
                let filter = if applicability == PolicyApplicability::RequiredSourceMissing {
                    bool_literal(false)
                } else {
                    let mut expr = constraint_ctx.scan_policy_base_expr(constraint)?;
                    if let Some(sink) = &context.sink {
                        expr = replace_sink_columns(expr, sink, &context.sink_expr_by_column);
                        expr = replace_sink_columns(expr, "_OUTPUT_", &context.sink_expr_by_column);
                        if let Some(sink_alias) = sink_alias {
                            expr = replace_sink_columns(
                                expr,
                                sink_alias,
                                &context.sink_expr_by_column,
                            );
                        }
                    }
                    rewrite_column_qualifiers(&mut expr, &table_scope.alias_by_base);
                    scan_policy_expr(
                        expr,
                        policy.sources(),
                        context,
                        &table_scope.alias_by_base,
                        constraint_ctx.uses_scan_ready_expr(),
                    )?
                };
                plan.actions.push(UpdatePolicyAction::CompatDfc {
                    filter,
                    on_fail: *on_fail,
                    description: description.clone(),
                });
            }
            PolicyIr::NativePgn(pgn) if pgn.kind == PgnPolicyKind::Update => {
                let filter = if applicability == PolicyApplicability::RequiredSourceMissing {
                    bool_literal(false)
                } else {
                    build_pgn_over_filter_expr(
                        &pgn.scope.sources,
                        &pgn.constraint,
                        &pgn.scope.sink_alias,
                        applicability,
                        context,
                        table_scope,
                        &constraint_ctx,
                    )?
                };
                plan.actions.push(UpdatePolicyAction::PgnUpdate {
                    filter,
                    on_fail: pgn.on_fail,
                    description: pgn.description.clone(),
                });
            }
            _ => {}
        }
    }
    plan.diagnostics.emitted_policy_actions = plan.actions.len();
    Ok(plan)
}

pub(crate) fn apply_update_scope_plan(
    plan: &UpdateScopePlan,
    assignments: &mut Vec<sqlparser::ast::Assignment>,
    selection: &mut Option<Expr>,
) -> Result<(), RewriteError> {
    use super::policy_expr::apply_update_resolution;
    for action in &plan.actions {
        match action {
            UpdatePolicyAction::CompatDfc {
                filter,
                on_fail,
                description,
            }
            | UpdatePolicyAction::PgnUpdate {
                filter,
                on_fail,
                description,
            } => {
                apply_update_resolution(
                    assignments,
                    selection,
                    filter.clone(),
                    *on_fail,
                    description.as_deref(),
                )?;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct MergeSourcePlan {
    pub diagnostics: ScopePlanDiagnostics,
    pub filters: Vec<Expr>,
}

#[derive(Debug, Clone, Default)]
pub struct InsertSinkInvalidationPlan {
    pub diagnostics: ScopePlanDiagnostics,
    pub append_valid: bool,
    pub append_invalid_string: bool,
}

pub(crate) fn plan_insert_sink_invalidation(
    store: &PolicyStore,
    sink: &str,
) -> InsertSinkInvalidationPlan {
    let sink_key = TableKey::new(sink);
    let candidate_ids = store.candidate_ids_for_sink(&sink_key);
    let mut plan = InsertSinkInvalidationPlan {
        diagnostics: ScopePlanDiagnostics {
            candidate_policies: candidate_ids.len(),
            ..ScopePlanDiagnostics::default()
        },
        ..InsertSinkInvalidationPlan::default()
    };
    for index in candidate_ids {
        let Some(policy) = store.policy(index) else {
            continue;
        };
        if !policy
            .sink()
            .is_some_and(|policy_sink| policy_sink.eq_ignore_ascii_case(sink))
        {
            continue;
        }
        plan.diagnostics.applicable_policies += 1;
        match policy.resolution() {
            Resolution::Invalidate => plan.append_valid = true,
            Resolution::InvalidateMessage => plan.append_invalid_string = true,
            _ => {}
        }
        if plan.append_valid && plan.append_invalid_string {
            break;
        }
    }
    plan
}

#[derive(Debug, Clone, Default)]
pub struct InsertAggregateColumnPlan {
    pub diagnostics: ScopePlanDiagnostics,
    pub temp_columns: Vec<(super::types::SourceAggregate, String)>,
}

pub(crate) fn plan_insert_aggregate_temp_columns(
    store: &PolicyStore,
    stats: Option<&RewriteStatsCell>,
    sink: &str,
    table_scope: &TableScope,
) -> Result<InsertAggregateColumnPlan, RewriteError> {
    use std::collections::HashSet;

    use super::aggregates::{aggregate_temp_column, policy_aggregate_temp_entries_from_expr};

    let mut plan = InsertAggregateColumnPlan::default();
    let mut seen = HashSet::new();
    let mut temp_columns = Vec::new();
    for index in store.aggregate_policy_indices_for_scope(sink, &table_scope.direct_base_tables) {
        let Some(PolicyIr::CompatAggregate(policy)) = store.policy(index) else {
            continue;
        };
        plan.diagnostics.candidate_policies += 1;
        if policy
            .sink
            .as_deref()
            .is_some_and(|policy_sink| !policy_sink.eq_ignore_ascii_case(sink))
        {
            continue;
        }
        if !policy.sources.is_empty()
            && !policy.sources.iter().all(|source| {
                table_scope
                    .direct_base_tables
                    .contains(&TableKey::new(source))
            })
        {
            continue;
        }
        plan.diagnostics.applicable_policies += 1;
        let constraint_ctx = ConstraintExprCtx {
            store,
            index,
            stats,
        };
        let constraint_expr = constraint_ctx.expr(&policy.constraint)?;
        for aggregate in policy_aggregate_temp_entries_from_expr(
            Some(&constraint_expr),
            &policy.constraint,
            &policy.sources,
            policy.sink.as_deref(),
        )? {
            if seen.insert(aggregate.sql.clone()) {
                temp_columns.push((aggregate, String::new()));
            }
        }
    }
    plan.temp_columns = temp_columns
        .into_iter()
        .enumerate()
        .map(|(column_index, (aggregate, _))| (aggregate, aggregate_temp_column(column_index + 1)))
        .collect();
    Ok(plan)
}

pub(crate) fn plan_merge_source_filters(
    store: &PolicyStore,
    stats: Option<&RewriteStatsCell>,
    source_tables: &HashSet<TableKey>,
) -> Result<MergeSourcePlan, RewriteError> {
    let mut plan = MergeSourcePlan::default();
    let candidate_ids = store.candidate_ids_for_tables(source_tables);
    plan.diagnostics.candidate_policies = candidate_ids.len();
    for index in candidate_ids {
        let Some(policy) = store.policy(index) else {
            continue;
        };
        let PolicyIr::CompatDfc {
            constraint,
            on_fail,
            ..
        } = policy
        else {
            continue;
        };
        if !matches!(on_fail, Resolution::Remove) {
            continue;
        }
        if !policy
            .sources()
            .iter()
            .all(|source| source_tables.contains(&TableKey::new(source)))
        {
            continue;
        }
        plan.diagnostics.applicable_policies += 1;
        let constraint_ctx = ConstraintExprCtx {
            store,
            index,
            stats,
        };
        let context = RewriteContext::default();
        let mut expr = constraint_ctx.scan_policy_base_expr(constraint)?;
        expr = scan_policy_expr(
            expr,
            policy.sources(),
            &context,
            &AliasByBase::default(),
            constraint_ctx.uses_scan_ready_expr(),
        )?;
        plan.filters.push(expr);
    }
    Ok(plan)
}

/// True when indexed lookup finds an enforcement policy for the scope (Partial-Push gate).
pub(crate) fn scope_has_enforcement_policies(
    store: &PolicyStore,
    direct_tables: &HashSet<TableKey>,
    exists_subquery_tables: &HashSet<TableKey>,
) -> bool {
    let mut candidate_tables = direct_tables.clone();
    candidate_tables.extend(exists_subquery_tables.iter().cloned());
    let candidate_lookup = store.enforcement_candidate_lookup(&candidate_tables);
    candidate_lookup.iter().any(|index| {
        let Some(compiled) = store.compiled(index) else {
            return false;
        };
        if compiled_policy_applicability(compiled, direct_tables, None, false).is_some() {
            return true;
        }
        compiled.source_keys.iter().any(|source| {
            exists_subquery_tables.contains(source) && !direct_tables.contains(source)
        })
    })
}

pub(crate) fn plan_select_rewrite(
    store: &PolicyStore,
    catalog: &TableCatalog,
    stats: Option<&RewriteStatsCell>,
    select: &Select,
    analysis: &SelectAnalysis,
    context: &RewriteContext,
    exists_handled: &HashSet<usize>,
) -> Result<SelectRewritePlan, RewriteError> {
    let mut plan = SelectRewritePlan {
        apply_aggregate_scan_columns: context.sink.is_none(),
        ..SelectRewritePlan::default()
    };

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

    plan.policy_actions = build_policy_resolution_actions(
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
) -> Result<(), RewriteError> {
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
) -> Result<(), RewriteError> {
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
        let PolicyIr::CompatDfc {
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
) -> Result<(), RewriteError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{PolicyIr, Resolution};
    use crate::rewriter::PassantRewriter;

    fn remove_policy(source: &str, constraint: &str) -> PolicyIr {
        PolicyIr::CompatDfc {
            sources: vec![source.to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: constraint.to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }
    }

    #[test]
    fn resolve_scope_policies_matches_indexed_candidates() {
        let mut rewriter = PassantRewriter::new();
        rewriter.register_policy(remove_policy("foo", "foo.id > 1"));
        rewriter.register_policy(remove_policy("bar", "bar.id > 1"));
        let tables = HashSet::from([TableKey::new("foo")]);
        let (applicable, diag) = resolve_scope_policies(
            rewriter.policy_store(),
            &tables,
            None,
            false,
            &HashSet::new(),
            &HashSet::new(),
        );
        assert_eq!(diag.candidate_policies, 1);
        assert_eq!(applicable.len(), 1);
        assert_eq!(applicable[0].1.sources(), &["foo"]);
    }

    #[test]
    fn plan_select_rewrite_emits_where_action_for_scan_policy() {
        let mut rewriter = PassantRewriter::new();
        rewriter.register_policy(remove_policy("foo", "foo.id > 1"));
        let statement = sqlparser::parser::Parser::parse_sql(
            &sqlparser::dialect::GenericDialect {},
            "SELECT id FROM foo",
        )
        .expect("parse")
        .pop()
        .expect("statement");
        let sqlparser::ast::Statement::Query(query) = statement else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = query.body.as_ref() else {
            panic!("expected select");
        };
        let analysis = SelectAnalysis::from_select(select);
        let context = RewriteContext::default();
        let plan = plan_select_rewrite(
            rewriter.policy_store(),
            rewriter.catalog(),
            None,
            select,
            &analysis,
            &context,
            &HashSet::new(),
        )
        .expect("plan");
        assert_eq!(plan.diagnostics.candidate_policies, 1);
        assert_eq!(plan.diagnostics.emitted_policy_actions, 1);
        assert!(matches!(
            plan.policy_actions.first(),
            Some(PolicyResolutionAction::CompatDfc { .. })
        ));
    }

    #[test]
    fn plan_statement_rewrite_summary_counts_each_select_scope() {
        let mut rewriter = PassantRewriter::new();
        rewriter.register_policy(remove_policy("foo", "foo.id > 1"));
        rewriter.register_policy(remove_policy("bar", "bar.id > 1"));
        let statement = sqlparser::parser::Parser::parse_sql(
            &sqlparser::dialect::GenericDialect {},
            "WITH cte AS (SELECT id FROM bar) SELECT id FROM foo JOIN cte ON foo.id = cte.id",
        )
        .expect("parse")
        .pop()
        .expect("statement");
        let summary = plan_statement_rewrite_summary(rewriter.policy_store(), &statement);
        assert_eq!(summary.scope_diagnostics.len(), 2);
        assert_eq!(summary.aggregate().candidate_policies, 2);
    }
}
