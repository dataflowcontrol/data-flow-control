//! Frame-aware policy aggregation for the supported window boundary.
//!
//! A window aggregate does not collapse rows. Its policy aggregate must use the
//! same frame, rather than becoming a GROUP BY aggregate or a row-local check.
use std::ops::ControlFlow;

use sqlparser::ast::{
    Expr, GroupByExpr, Select, Visit, VisitMut, Visitor, VisitorMut, WindowFrameBound, WindowType,
};

use crate::aggregate_registry::AggregateRegistry;
use crate::diagnostics::RewriteError;

use super::plan::{PolicyResolutionAction, SelectRewritePlan};
use super::resolution::PASSANT_KILL_UDF;

pub(crate) fn policy_window(
    select: &Select,
    registry: &AggregateRegistry,
) -> Result<Option<WindowType>, RewriteError> {
    struct Windows<'a> {
        registry: &'a AggregateRegistry,
        windows: Vec<WindowType>,
        grouped: bool,
        unsupported_value_window: bool,
    }
    impl Visitor for Windows<'_> {
        type Break = ();
        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
            if let Expr::Function(function) = expr {
                if let Some(window) = &function.over {
                    let name = function.name.to_string().to_ascii_lowercase();
                    if name == "row_number" {
                        // Preserve the established row-wise ROW_NUMBER case.
                    } else if matches!(
                        name.as_str(),
                        "lead"
                            | "lag"
                            | "first_value"
                            | "last_value"
                            | "nth_value"
                            | "rank"
                            | "dense_rank"
                            | "percent_rank"
                            | "cume_dist"
                            | "ntile"
                    ) || !self.registry.is_aggregate_call(function)
                    {
                        // DuckDB introspection also lists navigation/ranking
                        // functions in the aggregate registry. Their dependency
                        // sets are not ordinary aggregate frames.
                        self.unsupported_value_window = true;
                    } else {
                        self.windows.push(window.clone());
                    }
                } else if self.registry.is_aggregate_call(function) {
                    self.grouped = true;
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut found = Windows {
        registry,
        windows: Vec::new(),
        grouped: false,
        unsupported_value_window: false,
    };
    for projection in &select.projection {
        let _ = projection.visit(&mut found);
    }
    if let Some(qualify) = &select.qualify {
        let _ = qualify.visit(&mut found);
    }
    if found.unsupported_value_window {
        return Err(RewriteError::unsupported_statement(
            "policy-relevant value/navigation windows require contributor-frame provenance",
        ));
    }
    let Some(window) = found.windows.first() else {
        return Ok(None);
    };
    let grouped =
        !matches!(&select.group_by, GroupByExpr::Expressions(exprs, _) if exprs.is_empty());
    if grouped || found.grouped || select.having.is_some() || select.qualify.is_some() {
        return Err(RewriteError::unsupported_statement(
            "policy window aggregation does not support grouped, HAVING or QUALIFY inputs",
        ));
    }
    if found.windows.iter().any(|other| other != window) {
        return Err(RewriteError::unsupported_statement(
            "policy window aggregation requires one common contributor frame",
        ));
    }
    let WindowType::WindowSpec(spec) = window else {
        return Err(RewriteError::unsupported_statement(
            "policy window aggregation requires an inline window specification",
        ));
    };
    if spec.window_name.is_some() {
        return Err(RewriteError::unsupported_statement(
            "policy window aggregation requires an inline window specification",
        ));
    }
    if let Some(frame) = &spec.window_frame {
        let includes_current = matches!(
            frame.start_bound,
            WindowFrameBound::Preceding(_) | WindowFrameBound::CurrentRow
        ) && matches!(
            frame.end_bound,
            None | Some(WindowFrameBound::CurrentRow) | Some(WindowFrameBound::Following(_))
        );
        if !includes_current {
            return Err(RewriteError::unsupported_statement(
                "policy window aggregation requires a frame including the current row",
            ));
        }
    }
    Ok(Some(window.clone()))
}

pub(crate) fn apply_policy_window(
    plan: &mut SelectRewritePlan,
    window: &WindowType,
    registry: &AggregateRegistry,
) -> Result<(), RewriteError> {
    struct WindowAggregates<'a> {
        registry: &'a AggregateRegistry,
        window: &'a WindowType,
    }
    impl VisitorMut for WindowAggregates<'_> {
        type Break = ();
        fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<()> {
            if let Expr::Function(function) = expr
                && self.registry.is_aggregate_call(function)
                && function.over.is_none()
            {
                function.over = Some(self.window.clone());
            }
            ControlFlow::Continue(())
        }
    }
    // Filtering before evaluation changes the frame and can expose data from
    // rejected contributors. Only the existing terminal KILL wrapper is used.
    if !plan.table_factor_filters.is_empty()
        || !plan.join_on_filters.is_empty()
        || !plan.selection_filters.is_empty()
        || !plan.multi_source_join_pushes.is_empty()
    {
        return Err(RewriteError::unsupported_statement(
            "policy window aggregation currently requires KILL resolution",
        ));
    }
    for action in &mut plan.policy_actions {
        let PolicyResolutionAction::TupleUdf {
            filter, udf_name, ..
        } = action
        else {
            return Err(RewriteError::unsupported_statement(
                "policy window aggregation currently requires KILL resolution",
            ));
        };
        if !udf_name.eq_ignore_ascii_case(PASSANT_KILL_UDF) {
            return Err(RewriteError::unsupported_statement(
                "policy window aggregation currently requires KILL resolution",
            ));
        }
        let _ = filter.visit(&mut WindowAggregates { registry, window });
    }
    Ok(())
}
