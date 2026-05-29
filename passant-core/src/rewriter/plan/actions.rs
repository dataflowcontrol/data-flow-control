use std::collections::HashSet;

use sqlparser::ast::{Expr, Select};

use crate::catalog::TableCatalog;
use crate::identifiers::TableKey;
use crate::policy::{PolicyIr, Resolution};
use crate::policy_store::PolicyStore;
use crate::rewrite_stats::RewriteStatsCell;
use crate::rewriter::dimensions::{
    constraint_references_skipped_dimensions, inject_policy_dimensions,
};
use crate::rewriter::expr::bool_literal;
use crate::rewriter::expr::{add_filter, and_expr};
use crate::rewriter::policy_expr::{
    ConstraintExprCtx, build_pgn_filter_expr, unique_column_guard_from_constraint,
};
use crate::rewriter::resolution::{
    PASSANT_KILL_UDF, UiResolutionSpec, ui_pass_filter, wrap_select_with_tuple_resolution,
};
use crate::rewriter::scope::TableScope;
use crate::rewriter::types::{PolicyApplicability, RewriteContext, UiResolutionMode};

use super::applicability::{ScopePlanDiagnostics, resolve_scope_policies};

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum PolicyResolutionAction {
    Filter {
        filter: Expr,
        #[allow(dead_code)]
        description: Option<String>,
    },
    TupleUdf {
        filter: Expr,
        udf_name: String,
        #[allow(dead_code)]
        description: Option<String>,
    },
    RelationUdf {
        filter: Expr,
        udf_name: String,
        #[allow(dead_code)]
        description: Option<String>,
    },
    Ui {
        filter: Expr,
        spec: UiResolutionSpec,
        policy: PolicyIr,
    },
}

pub(crate) fn action_for_resolution(
    filter: Expr,
    on_fail: &Resolution,
    description: Option<String>,
    policy: &PolicyIr,
    policy_index: usize,
) -> Result<PolicyResolutionAction, crate::diagnostics::RewriteError> {
    match on_fail {
        Resolution::Remove => Ok(PolicyResolutionAction::Filter {
            filter,
            description,
        }),
        Resolution::Kill => Ok(PolicyResolutionAction::TupleUdf {
            filter,
            udf_name: PASSANT_KILL_UDF.to_string(),
            description,
        }),
        Resolution::Udf(name) => Ok(PolicyResolutionAction::TupleUdf {
            filter,
            udf_name: name.clone(),
            description,
        }),
        Resolution::RelationUdf(name) => Ok(PolicyResolutionAction::RelationUdf {
            filter,
            udf_name: name.clone(),
            description,
        }),
        Resolution::Ui => Ok(PolicyResolutionAction::Ui {
            filter,
            spec: UiResolutionSpec {
                constraint: policy.constraint().to_string(),
                description,
                sink: policy.sink().map(str::to_string),
                policy_index,
            },
            policy: policy.clone(),
        }),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_policy_resolution_actions(
    store: &PolicyStore,
    catalog: &TableCatalog,
    stats: Option<&RewriteStatsCell>,
    select: &mut Select,
    context: &RewriteContext,
    is_aggregation: bool,
    applicable: Vec<(usize, &PolicyIr, PolicyApplicability)>,
    diagnostics: &mut ScopePlanDiagnostics,
) -> Result<Vec<PolicyResolutionAction>, crate::diagnostics::RewriteError> {
    if is_aggregation
        && applicable
            .iter()
            .any(|(_, policy, _)| policy.resolution() == Resolution::Ui)
    {
        return Err(crate::diagnostics::RewriteError::unsupported_statement(
            "UI resolution for aggregate INSERT ... SELECT is not supported yet",
        ));
    }

    let mut actions = Vec::new();
    for (index, policy, applicability) in applicable {
        let skipped_dimensions =
            inject_policy_dimensions(select, policy, catalog, &mut diagnostics.warnings)?;
        let table_scope = TableScope::from_select(select);
        let constraint_ctx = ConstraintExprCtx {
            store,
            index,
            stats,
        };
        let PolicyIr::Pgn {
            sources,
            constraint,
            on_fail,
            sink_alias,
            source_aliases,
            description,
            dimension_aliases,
            ..
        } = policy;
        let mut expr = if constraint_references_skipped_dimensions(
            constraint,
            &skipped_dimensions,
            dimension_aliases,
        ) {
            bool_literal(false)
        } else {
            build_pgn_filter_expr(
                sources,
                constraint,
                sink_alias,
                source_aliases,
                applicability,
                context,
                &table_scope,
                is_aggregation,
                &constraint_ctx,
            )?
        };
        if is_aggregation
            && let Some(guard) =
                unique_column_guard_from_constraint(constraint, sources, &constraint_ctx)
        {
            expr = and_expr(guard, expr);
        }
        actions.push(action_for_resolution(
            expr,
            on_fail,
            description.clone(),
            policy,
            index,
        )?);
    }
    Ok(actions)
}

/// Build policy filter actions for a scope (shared by Full-Push, Partial-Push, HAVING).
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_policy_filter_actions(
    store: &PolicyStore,
    catalog: &TableCatalog,
    stats: Option<&RewriteStatsCell>,
    select: &mut Select,
    direct_tables: &HashSet<TableKey>,
    sink: Option<&str>,
    context: &RewriteContext,
    is_aggregation: bool,
    exclude_pushdown: &HashSet<usize>,
    exclude_exists: &HashSet<usize>,
) -> Result<(Vec<PolicyResolutionAction>, ScopePlanDiagnostics), crate::diagnostics::RewriteError> {
    let (applicable, mut diagnostics) = resolve_scope_policies(
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
        select,
        context,
        is_aggregation,
        applicable,
        &mut diagnostics,
    )?;
    Ok((actions, diagnostics))
}

pub(crate) fn apply_policy_resolution_actions(
    select: &mut Select,
    actions: &[PolicyResolutionAction],
    is_aggregation: bool,
    context: &RewriteContext,
    store: &PolicyStore,
    catalog: &TableCatalog,
) -> Result<(), crate::diagnostics::RewriteError> {
    let ui_count = actions
        .iter()
        .filter(|action| matches!(action, PolicyResolutionAction::Ui { .. }))
        .count();
    if ui_count > 1 {
        return Err(crate::diagnostics::RewriteError::unsupported_statement(
            "multiple UI resolution policies in one SELECT scope are not supported yet",
        ));
    }

    for action in actions {
        match action {
            PolicyResolutionAction::Filter { filter, .. } => {
                add_filter(select, filter.clone(), is_aggregation)?;
            }
            PolicyResolutionAction::TupleUdf {
                filter, udf_name, ..
            } => {
                let inner = std::mem::replace(select, crate::sql::empty_select());
                *select = wrap_select_with_tuple_resolution(inner, filter.clone(), udf_name)?;
            }
            PolicyResolutionAction::RelationUdf { .. } => {}
            PolicyResolutionAction::Ui {
                filter,
                spec,
                policy,
            } => {
                validate_ui_resolution_context(context, spec.sink.as_deref())?;
                let table_scope = TableScope::from_select(select);
                let ui_filter = ui_pass_filter(
                    filter.clone(),
                    select,
                    spec,
                    store,
                    policy,
                    context,
                    &table_scope,
                    catalog,
                )?;
                add_filter(select, ui_filter, is_aggregation)?;
            }
        }
    }
    Ok(())
}

fn validate_ui_resolution_context(
    context: &RewriteContext,
    policy_sink: Option<&str>,
) -> Result<(), crate::diagnostics::RewriteError> {
    match context.ui_mode {
        UiResolutionMode::InsertSelect => {
            let Some(insert_sink) = context.sink.as_deref() else {
                return Err(crate::diagnostics::RewriteError::unsupported_statement(
                    "UI resolution for INSERT ... SELECT requires an insert target",
                ));
            };
            let Some(policy_sink) = policy_sink else {
                return Err(crate::diagnostics::RewriteError::unsupported_statement(
                    "UI resolution for INSERT ... SELECT requires a policy sink",
                ));
            };
            if TableKey::new(insert_sink) != TableKey::new(policy_sink) {
                return Err(crate::diagnostics::RewriteError::unsupported_statement(
                    format!(
                        "UI policy sink {policy_sink} does not match insert target {insert_sink}"
                    ),
                ));
            }
            Ok(())
        }
        UiResolutionMode::SelectResult | UiResolutionMode::Disabled => Ok(()),
        UiResolutionMode::UpdateApprovalOnly | UiResolutionMode::UpdateEditedRows => {
            let Some(update_sink) = context.sink.as_deref() else {
                return Err(crate::diagnostics::RewriteError::unsupported_statement(
                    "UI resolution for UPDATE requires an update target table",
                ));
            };
            if let Some(policy_sink) = policy_sink
                && TableKey::new(update_sink) != TableKey::new(policy_sink)
            {
                return Err(crate::diagnostics::RewriteError::unsupported_statement(
                    format!(
                        "UI policy sink {policy_sink} does not match update target {update_sink}"
                    ),
                ));
            }
            Ok(())
        }
    }
}

pub(crate) fn relation_violation_filters(actions: &[PolicyResolutionAction]) -> Vec<Expr> {
    actions
        .iter()
        .filter_map(|action| match action {
            PolicyResolutionAction::RelationUdf { filter, .. } => Some(filter.clone()),
            _ => None,
        })
        .collect()
}

pub(crate) fn relation_udf_names(actions: &[PolicyResolutionAction]) -> Vec<String> {
    actions
        .iter()
        .filter_map(|action| match action {
            PolicyResolutionAction::RelationUdf { udf_name, .. } => Some(udf_name.clone()),
            _ => None,
        })
        .collect()
}
