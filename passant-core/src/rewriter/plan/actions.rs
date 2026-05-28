use std::collections::HashSet;

use sqlparser::ast::{Expr, Select};

use crate::catalog::TableCatalog;
use crate::identifiers::TableKey;
use crate::policy::{PgnPolicyKind, PolicyIr, Resolution};
use crate::policy_store::PolicyStore;
use crate::rewrite_stats::RewriteStatsCell;
use crate::rewriter::expr::{and_expr, apply_resolution};
use crate::rewriter::policy_expr::{
    ConstraintExprCtx, build_dfc_filter_expr, build_pgn_over_filter_expr,
    unique_column_guard_from_constraint,
};
use crate::rewriter::scope::TableScope;
use crate::rewriter::types::{PolicyApplicability, RewriteContext};

use super::applicability::{ScopePlanDiagnostics, resolve_scope_policies};

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum PolicyResolutionAction {
    Dfc {
        filter: Expr,
        on_fail: Resolution,
        description: Option<String>,
    },
    PgnOver {
        filter: Expr,
        on_fail: Resolution,
        description: Option<String>,
    },
}

pub(crate) fn build_policy_resolution_actions(
    store: &PolicyStore,
    catalog: &TableCatalog,
    stats: Option<&RewriteStatsCell>,
    table_scope: &TableScope,
    context: &RewriteContext,
    is_aggregation: bool,
    applicable: Vec<(usize, &PolicyIr, PolicyApplicability)>,
) -> Result<Vec<PolicyResolutionAction>, crate::diagnostics::RewriteError> {
    let mut actions = Vec::new();
    for (index, policy, applicability) in applicable {
        let constraint_ctx = ConstraintExprCtx {
            store,
            index,
            stats,
        };
        match policy {
            PolicyIr::Dfc {
                sources,
                constraint,
                on_fail,
                sink_alias,
                description,
                ..
            } => {
                let mut expr = build_dfc_filter_expr(
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
                actions.push(PolicyResolutionAction::Dfc {
                    filter: expr,
                    on_fail: *on_fail,
                    description: description.clone(),
                });
            }
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
) -> Result<(Vec<PolicyResolutionAction>, ScopePlanDiagnostics), crate::diagnostics::RewriteError> {
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
) -> Result<(), crate::diagnostics::RewriteError> {
    for action in actions {
        match action {
            PolicyResolutionAction::Dfc {
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
                )?;
            }
        }
    }
    Ok(())
}
