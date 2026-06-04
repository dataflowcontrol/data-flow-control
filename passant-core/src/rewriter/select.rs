use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Query, Select, SelectItem, SetExpr,
    SetOperator, TableFactor, TableWithJoins,
};
use std::time::Instant;

use crate::identifiers::TableKey;
use crate::query_analysis::SelectAnalysis;
use crate::source_sets::{
    cross_source_policies_for_branch_indexed, set_expr_source_tables,
    set_operation_requires_cross_source_policies_for_store,
    split_select_policies_for_nullable_joins_for_store, split_set_operation_policies_for_store,
};
use crate::sql::ast_stats::count_select;

use super::RewriteError;
use super::derived_policy::{
    apply_derived_hidden_projections, apply_derived_parent_having, plan_derived_policy_propagations,
};
use super::exists::apply_in_semijoin_policy_filters;
use super::helpers::select_output_column_mapping;
use super::limit::wrap_limited_policy_query;
use super::plan::{apply_select_rewrite_plan, plan_select_rewrite};
use super::types::{PassantRewriter, RewriteContext};

impl PassantRewriter {
    pub(crate) fn rewrite_expr_subqueries(
        &self,
        expr: &mut Expr,
        context: &mut RewriteContext,
    ) -> Result<(), RewriteError> {
        match expr {
            Expr::Exists {
                subquery,
                negated: true,
            }
            | Expr::InSubquery {
                subquery,
                negated: true,
                ..
            } => self.rewrite_query_with_context(subquery, context),
            Expr::Exists { subquery, .. } | Expr::InSubquery { subquery, .. } => {
                self.rewrite_query_with_context(subquery, context)
            }
            Expr::Subquery(subquery) => self.rewrite_query_with_context(subquery, context),
            Expr::BinaryOp { left, right, .. } => {
                self.rewrite_expr_subqueries(left, context)?;
                self.rewrite_expr_subqueries(right, context)
            }
            Expr::Nested(expr)
            | Expr::UnaryOp { expr, .. }
            | Expr::IsFalse(expr)
            | Expr::IsNotFalse(expr)
            | Expr::IsTrue(expr)
            | Expr::IsNotTrue(expr)
            | Expr::IsNull(expr)
            | Expr::IsNotNull(expr) => self.rewrite_expr_subqueries(expr, context),
            Expr::Function(function) => self.rewrite_function_subqueries(function, context),
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => {
                if let Some(operand) = operand {
                    self.rewrite_expr_subqueries(operand, context)?;
                }
                for expr in conditions.iter_mut().chain(results.iter_mut()) {
                    self.rewrite_expr_subqueries(expr, context)?;
                }
                if let Some(else_result) = else_result {
                    self.rewrite_expr_subqueries(else_result, context)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn rewrite_function_subqueries(
        &self,
        function: &mut sqlparser::ast::Function,
        context: &mut RewriteContext,
    ) -> Result<(), RewriteError> {
        let FunctionArguments::List(args) = &mut function.args else {
            return Ok(());
        };
        for arg in &mut args.args {
            match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(expr),
                    ..
                }
                | FunctionArg::ExprNamed {
                    arg: FunctionArgExpr::Expr(expr),
                    ..
                } => self.rewrite_expr_subqueries(expr, context)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn rewrite_set_operation_with_cross_source_fallback(
        &self,
        left: &mut SetExpr,
        right: &mut SetExpr,
        context: &mut RewriteContext,
    ) -> Result<(), RewriteError> {
        let left_tables = set_expr_source_tables(left);
        let right_tables = set_expr_source_tables(right);
        let left_policies =
            cross_source_policies_for_branch_indexed(self.policy_store(), &left_tables);
        let right_policies =
            cross_source_policies_for_branch_indexed(self.policy_store(), &right_tables);
        let mut branch_context = context.clone();
        branch_context.allow_partial_source_visibility = true;
        PassantRewriter::with_branch_view(
            self.policy_store(),
            left_policies,
            self.catalog.clone(),
            self.parse_dialect,
        )
        .rewrite_set_expr(left, &mut branch_context)?;
        PassantRewriter::with_branch_view(
            self.policy_store(),
            right_policies,
            self.catalog.clone(),
            self.parse_dialect,
        )
        .rewrite_set_expr(right, &mut branch_context)
    }

    fn rewrite_select(
        &self,
        select: &mut Select,
        context: &mut RewriteContext,
    ) -> Result<(), RewriteError> {
        if context.collect_stats {
            self.stats
                .add_ast_nodes_visited_rewrite(count_select(select));
        }
        let exists_handled = self.rewrite_exists_subqueries_as_joins_impl(select)?;
        let (in_handled, extra_dfc) = self.rewrite_in_subqueries_as_joins_impl(select)?;
        let mut handled = exists_handled;
        handled.extend(in_handled.iter().copied());

        let derived_prep = plan_derived_policy_propagations(
            self.policy_store(),
            select,
            &self.aggregate_registry,
        )?;
        if let Some(ref prep) = derived_prep {
            apply_derived_hidden_projections(
                select,
                prep,
                self.policy_store(),
                &self.aggregate_registry,
            )?;
            context
                .deferred_policy_indices
                .extend(prep.deferred_indices.iter().copied());
        }

        self.rewrite_expression_subqueries(select, context)?;
        self.rewrite_derived_subqueries(select, context)?;

        let select_analysis = SelectAnalysis::from_select(select);
        let is_aggregation = select_analysis.is_aggregation;

        if !extra_dfc.is_empty() {
            apply_in_semijoin_policy_filters(select, &extra_dfc, is_aggregation, context)?;
        }

        if context.collect_stats {
            self.stats.record_select_scope();
        }
        let sink_key = context.sink.as_ref().map(|sink| TableKey::new(sink));
        if let Some(entries) = split_select_policies_for_nullable_joins_for_store(
            &self.store,
            select,
            &select_analysis.scope.direct_base_tables,
            sink_key.as_ref(),
        ) {
            return PassantRewriter::with_branch_view(
                self.policy_store(),
                entries,
                self.catalog.clone(),
                self.parse_dialect,
            )
            .rewrite_select(select, context);
        }

        let stats = context.collect_stats.then_some(&self.stats);
        let plan_context = {
            let mut local_context = context.clone();
            if local_context.sink_expr_by_column.is_empty() {
                let output_mapping = select_output_column_mapping(select, &self.catalog)?;
                local_context.sink_expr_by_column = output_mapping.expr_by_column;
                local_context.ambiguous_output_columns = output_mapping.ambiguous_columns;
            }
            local_context
        };
        let plan_start = Instant::now();
        let plan = plan_select_rewrite(
            &self.store,
            &self.catalog,
            stats,
            select,
            &select_analysis,
            &plan_context,
            &handled,
        )?;
        if context.collect_stats {
            self.stats.add_elapsed_planning(plan_start.elapsed());
        }
        self.statement_summary
            .record_scope(plan.diagnostics.clone());
        if context.collect_stats {
            self.stats.accumulate_scope_diagnostics(
                plan.diagnostics.candidate_policies,
                plan.diagnostics.applicable_policies,
                plan.diagnostics.dominated_policies,
            );
        }

        apply_select_rewrite_plan(
            select,
            plan,
            is_aggregation,
            &plan_context,
            &self.store,
            &self.catalog,
        )?;

        if let Some(ref prep) = derived_prep {
            apply_derived_parent_having(select, prep)?;
        }
        Ok(())
    }

    fn rewrite_expression_subqueries(
        &self,
        select: &mut Select,
        context: &mut RewriteContext,
    ) -> Result<(), RewriteError> {
        for item in &mut select.projection {
            match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    self.rewrite_expr_subqueries(expr, context)?;
                }
                _ => {}
            }
        }
        if let Some(selection) = &mut select.selection {
            self.rewrite_expr_subqueries(selection, context)?;
        }
        if let Some(having) = &mut select.having {
            self.rewrite_expr_subqueries(having, context)?;
        }
        Ok(())
    }

    pub(crate) fn rewrite_derived_table_factor(
        &self,
        factor: &mut TableFactor,
        context: &mut RewriteContext,
    ) -> Result<(), RewriteError> {
        if let TableFactor::Derived {
            subquery, alias, ..
        } = factor
        {
            if alias
                .as_ref()
                .is_some_and(|alias| alias.name.value.eq_ignore_ascii_case("exists_subquery"))
            {
                return Ok(());
            }
            self.rewrite_query_with_context(subquery, context)?;
        }
        Ok(())
    }

    fn rewrite_derived_table_with_joins(
        &self,
        table: &mut TableWithJoins,
        context: &mut RewriteContext,
    ) -> Result<(), RewriteError> {
        self.rewrite_derived_table_factor(&mut table.relation, context)?;
        for join in &mut table.joins {
            self.rewrite_derived_table_factor(&mut join.relation, context)?;
        }
        Ok(())
    }

    fn rewrite_derived_subqueries(
        &self,
        select: &mut Select,
        context: &mut RewriteContext,
    ) -> Result<(), RewriteError> {
        for table in &mut select.from {
            self.rewrite_derived_table_with_joins(table, context)?;
        }
        Ok(())
    }

    pub(crate) fn rewrite_set_expr(
        &self,
        set_expr: &mut SetExpr,
        context: &mut RewriteContext,
    ) -> Result<(), RewriteError> {
        match set_expr {
            SetExpr::Select(select) => self.rewrite_select(select, context),
            SetExpr::Query(query) => self.rewrite_query_with_context(query, context),
            SetExpr::SetOperation {
                op, left, right, ..
            } => {
                if set_operation_requires_cross_source_policies_for_store(
                    self.policy_store(),
                    left,
                    right,
                ) {
                    let Some((left_policies, right_policies)) =
                        split_set_operation_policies_for_store(self.policy_store(), left, right)
                    else {
                        if matches!(op, SetOperator::Except) {
                            self.rewrite_set_operation_with_cross_source_fallback(
                                left, right, context,
                            )?;
                        }
                        return Ok(());
                    };
                    PassantRewriter::with_branch_view(
                        self.policy_store(),
                        left_policies,
                        self.catalog.clone(),
                        self.parse_dialect,
                    )
                    .rewrite_set_expr(left, context)?;
                    PassantRewriter::with_branch_view(
                        self.policy_store(),
                        right_policies,
                        self.catalog.clone(),
                        self.parse_dialect,
                    )
                    .rewrite_set_expr(right, context)?;
                    return Ok(());
                }
                self.rewrite_set_expr(left, context)?;
                self.rewrite_set_expr(right, context)
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn rewrite_query_with_context(
        &self,
        query: &mut Query,
        context: &mut RewriteContext,
    ) -> Result<(), RewriteError> {
        if let Some(with) = query.with.as_mut() {
            for cte in &mut with.cte_tables {
                self.rewrite_query_with_context(&mut cte.query, context)?;
            }
        }
        if let Some(wrapped) = wrap_limited_policy_query(self, query, context)? {
            *query = wrapped;
            if context.sink.is_none() {
                self.apply_query_relation_resolution(query, context)?;
            }
            return Ok(());
        }
        self.rewrite_set_expr(query.body.as_mut(), context)?;
        if context.sink.is_none() {
            self.apply_query_relation_resolution(query, context)?;
        }
        Ok(())
    }
}
