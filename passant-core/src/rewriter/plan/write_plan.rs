use std::collections::HashSet;

use sqlparser::ast::Expr;

use crate::catalog::TableCatalog;
use crate::identifiers::{AliasByBase, TableKey};
use crate::policy::{PgnPolicyKind, PolicyIr, Resolution};
use crate::policy_store::PolicyStore;
use crate::rewrite_stats::RewriteStatsCell;
use crate::rewriter::columns::{replace_sink_columns, rewrite_column_qualifiers};
use crate::rewriter::expr::bool_literal;
use crate::rewriter::policy_expr::{
    ConstraintExprCtx, build_pgn_over_filter_expr, scan_policy_expr,
};
use crate::rewriter::scope::TableScope;
use crate::rewriter::types::{PolicyApplicability, RewriteContext};

use super::applicability::{ScopePlanDiagnostics, resolve_scope_policies};

#[derive(Debug, Clone)]
pub enum UpdatePolicyAction {
    Dfc {
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
) -> Result<UpdateScopePlan, crate::diagnostics::RewriteError> {
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
            PolicyIr::Dfc {
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
                plan.actions.push(UpdatePolicyAction::Dfc {
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
    assignments: &mut [sqlparser::ast::Assignment],
    selection: &mut Option<Expr>,
) -> Result<(), crate::diagnostics::RewriteError> {
    use crate::rewriter::policy_expr::apply_update_resolution;
    for action in &plan.actions {
        match action {
            UpdatePolicyAction::Dfc {
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

pub(crate) fn plan_merge_source_filters(
    store: &PolicyStore,
    stats: Option<&RewriteStatsCell>,
    source_tables: &HashSet<TableKey>,
) -> Result<MergeSourcePlan, crate::diagnostics::RewriteError> {
    let mut plan = MergeSourcePlan::default();
    let candidate_ids = store.candidate_ids_for_tables(source_tables);
    plan.diagnostics.candidate_policies = candidate_ids.len();
    for index in candidate_ids {
        let Some(policy) = store.policy(index) else {
            continue;
        };
        let PolicyIr::Dfc {
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
