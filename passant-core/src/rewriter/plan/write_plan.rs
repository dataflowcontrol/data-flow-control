use std::collections::HashSet;

use sqlparser::ast::Expr;

use crate::catalog::TableCatalog;
use crate::identifiers::{AliasByBase, TableKey};
use crate::policy::{PolicyIr, Resolution};
use crate::policy_store::PolicyStore;
use crate::rewrite_stats::RewriteStatsCell;
use crate::rewriter::columns::{
    apply_output_marker_replacements, apply_policy_sink_column_replacements,
    replace_source_alias_qualifiers, rewrite_column_qualifiers,
};
use crate::rewriter::expr::bool_literal;
use crate::rewriter::policy_expr::{ConstraintExprCtx, scan_policy_expr};
use crate::rewriter::scope::TableScope;
use crate::rewriter::types::{PolicyApplicability, RewriteContext};

use super::applicability::{ScopePlanDiagnostics, resolve_scope_policies};

#[derive(Debug, Clone)]
pub enum UpdatePolicyAction {
    Pgn {
        policy_index: usize,
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
        let PolicyIr::Pgn {
            constraint,
            on_fail,
            sink_alias,
            source_aliases,
            description,
            ..
        } = policy;
        let filter = if applicability == PolicyApplicability::RequiredSourceMissing {
            bool_literal(false)
        } else {
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
            rewrite_column_qualifiers(&mut expr, &table_scope.alias_by_base);
            scan_policy_expr(
                expr,
                policy.sources(),
                context,
                &table_scope.alias_by_base,
                constraint_ctx.uses_scan_ready_expr(),
                false,
            )?
        };
        plan.actions.push(UpdatePolicyAction::Pgn {
            policy_index: index,
            filter,
            on_fail: on_fail.clone(),
            description: description.clone(),
        });
    }
    plan.diagnostics.emitted_policy_actions = plan.actions.len();
    Ok(plan)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_update_scope_plan(
    plan: &UpdateScopePlan,
    assignments: &mut [sqlparser::ast::Assignment],
    selection: &mut Option<Expr>,
    context: &RewriteContext,
    store: &PolicyStore,
    table_scope: &TableScope,
    catalog: &TableCatalog,
    target_table: &str,
    ui_followup: &crate::rewriter::types::UiFollowupCell,
) -> Result<(), crate::diagnostics::RewriteError> {
    use crate::rewriter::policy_expr::apply_update_resolution;
    for action in &plan.actions {
        let UpdatePolicyAction::Pgn {
            policy_index,
            filter,
            on_fail,
            description,
        } = action;
        let Some(policy) = store.policy(*policy_index) else {
            continue;
        };
        apply_update_resolution(
            assignments,
            selection,
            filter.clone(),
            on_fail.clone(),
            description.as_deref(),
            context,
            store,
            *policy_index,
            policy,
            table_scope,
            catalog,
            target_table,
            ui_followup,
        )?;
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
        let PolicyIr::Pgn {
            constraint,
            on_fail,
            ..
        } = policy;
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
            false,
        )?;
        plan.filters.push(expr);
    }
    Ok(plan)
}
