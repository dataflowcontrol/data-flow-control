use std::collections::{HashMap, HashSet};

use sqlparser::ast::{
    Assignment, Expr, MergeAction, SetExpr, Statement, TableFactor, TableWithJoins,
};

use crate::source_sets::table_factor_source_tables;

use super::RewriteError;
use super::expr::{filter_table_factor, join_conjuncts};
use super::helpers::{insert_select_mapping, update_output_column_mapping, update_target_name};
use super::plan::{
    apply_update_scope_plan, plan_merge_source_filters, plan_policy_filter_actions,
    plan_update_scope, relation_udf_names, relation_violation_filters,
};
use super::resolution::{combine_violation_exprs, wrap_query_with_relation_resolution};
use super::scope::TableScope;
use super::types::{PassantRewriter, RewriteContext, RewriteOptions, UiResolutionMode};

impl PassantRewriter {
    fn rewrite_merge(
        &self,
        _target: &TableFactor,
        source: &mut TableFactor,
        _on: &mut Expr,
        clauses: &mut [sqlparser::ast::MergeClause],
        collect_stats: bool,
    ) -> Result<(), RewriteError> {
        self.rewrite_derived_table_factor(source, &RewriteContext::default())?;
        let source_tables = table_factor_source_tables(source);
        let stats = collect_stats.then_some(&self.stats);
        let merge_plan = plan_merge_source_filters(&self.store, stats, &source_tables)?;
        self.statement_summary
            .record_scope(merge_plan.diagnostics.clone());
        if collect_stats {
            self.stats.accumulate_scope_diagnostics(
                merge_plan.diagnostics.candidate_policies,
                merge_plan.diagnostics.applicable_policies,
                0,
            );
        }
        if !merge_plan.filters.is_empty() {
            filter_table_factor(source, join_conjuncts(merge_plan.filters))?;
        }
        for clause in clauses {
            if let MergeAction::Update { assignments } = &mut clause.action {
                for assignment in assignments {
                    self.rewrite_expr_subqueries(
                        &mut assignment.value,
                        &RewriteContext::default(),
                    )?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn rewrite_statement(
        &self,
        statement: &mut Statement,
        options: &RewriteOptions,
    ) -> Result<(), RewriteError> {
        let collect_stats = options.collect_stats;
        match statement {
            Statement::Query(query) => {
                let ui_mode = if self.has_ui_policies() {
                    UiResolutionMode::SelectResult
                } else {
                    UiResolutionMode::Disabled
                };
                let context = RewriteContext {
                    sink: None,
                    sink_expr_by_column: HashMap::new(),
                    ambiguous_output_columns: HashSet::new(),
                    allow_partial_source_visibility: false,
                    collect_stats,
                    ui_mode,
                    ui_stream_endpoint: options.ui_stream_endpoint.clone(),
                };
                self.rewrite_query_with_context(query, &context)
            }
            Statement::Insert(insert) => {
                let sink = insert.table_name.to_string();
                if insert.source.is_none() {
                    return Ok(());
                }
                self.expand_insert_columns_from_catalog(insert, &sink);
                let output_mapping = insert_select_mapping(insert)?;
                let context = RewriteContext {
                    sink: Some(sink),
                    sink_expr_by_column: output_mapping.expr_by_column,
                    ambiguous_output_columns: output_mapping.ambiguous_columns,
                    allow_partial_source_visibility: false,
                    collect_stats,
                    ui_mode: UiResolutionMode::InsertSelect,
                    ui_stream_endpoint: options.ui_stream_endpoint.clone(),
                };
                if let Some(source) = insert.source.as_mut() {
                    self.rewrite_query_with_context(source, &context)?;
                    self.apply_insert_relation_resolution(source, &context)?;
                }
                Ok(())
            }
            Statement::Update {
                table,
                assignments,
                from,
                selection,
                ..
            } => self.rewrite_update(table, assignments, from.as_ref(), selection, options),
            Statement::Merge {
                table,
                source,
                on,
                clauses,
                ..
            } => self.rewrite_merge(table, source, on, clauses, collect_stats),
            _ => Ok(()),
        }
    }

    fn apply_insert_relation_resolution(
        &self,
        query: &mut sqlparser::ast::Query,
        context: &RewriteContext,
    ) -> Result<(), RewriteError> {
        self.apply_query_relation_resolution(query, context)
    }

    pub(crate) fn apply_query_relation_resolution(
        &self,
        query: &mut sqlparser::ast::Query,
        context: &RewriteContext,
    ) -> Result<(), RewriteError> {
        let SetExpr::Select(select) = query.body.as_ref() else {
            return Ok(());
        };
        let mut plan_select = select.as_ref().clone();
        let table_scope = TableScope::from_select(&plan_select);
        let is_aggregation = super::projection::select_is_aggregation(&plan_select);
        let stats = context.collect_stats.then_some(&self.stats);
        let (actions, _) = plan_policy_filter_actions(
            &self.store,
            &self.catalog,
            stats,
            &mut plan_select,
            &table_scope.direct_base_tables,
            context.sink.as_deref(),
            context,
            is_aggregation,
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )?;
        let violation_filters = relation_violation_filters(&actions);
        if violation_filters.is_empty() {
            return Ok(());
        }
        let udf_names = relation_udf_names(&actions);
        if udf_names.len() != 1 {
            return Err(RewriteError::unsupported_statement(
                "multiple relation UDF resolutions in one scope are not supported",
            ));
        }
        let violation = combine_violation_exprs(violation_filters);
        let inner = query.clone();
        *query = wrap_query_with_relation_resolution(inner, violation, &udf_names[0])?;
        Ok(())
    }

    fn expand_insert_columns_from_catalog(&self, insert: &mut sqlparser::ast::Insert, sink: &str) {
        if !insert.columns.is_empty() {
            return;
        }
        let Some(source) = insert.source.as_ref() else {
            return;
        };
        if !matches!(source.body.as_ref(), SetExpr::Select(_)) {
            return;
        }
        let Some(catalog_columns) = self.catalog.columns(sink) else {
            return;
        };
        insert.columns = catalog_columns
            .iter()
            .map(|column| sqlparser::ast::Ident::new(column.as_str()))
            .collect();
    }

    fn rewrite_update(
        &self,
        table: &TableWithJoins,
        assignments: &mut [Assignment],
        from: Option<&TableWithJoins>,
        selection: &mut Option<Expr>,
        options: &RewriteOptions,
    ) -> Result<(), RewriteError> {
        let collect_stats = options.collect_stats;
        let sink = update_target_name(table);
        let mut table_scope = TableScope::default();
        table_scope.add_table_with_joins(table);
        if let Some(from) = from {
            table_scope.add_table_with_joins(from);
        }
        let output_mapping = update_output_column_mapping(table, assignments, &self.catalog)?;
        let ui_mode = if self.has_ui_policies() {
            match options.ui_update_mode {
                crate::rewriter::types::UiUpdateMode::EditedRows => {
                    UiResolutionMode::UpdateEditedRows
                }
                crate::rewriter::types::UiUpdateMode::ApprovalOnly => {
                    UiResolutionMode::UpdateApprovalOnly
                }
            }
        } else {
            UiResolutionMode::Disabled
        };
        let context = RewriteContext {
            sink: sink.clone(),
            sink_expr_by_column: output_mapping.expr_by_column,
            ambiguous_output_columns: output_mapping.ambiguous_columns,
            allow_partial_source_visibility: false,
            collect_stats,
            ui_mode,
            ui_stream_endpoint: options.ui_stream_endpoint.clone(),
        };
        let stats = context.collect_stats.then_some(&self.stats);
        let update_plan = plan_update_scope(
            &self.store,
            &self.catalog,
            stats,
            &table_scope,
            sink.as_deref(),
            &context,
        )?;
        self.statement_summary
            .record_scope(update_plan.diagnostics.clone());
        if context.collect_stats {
            self.stats.accumulate_scope_diagnostics(
                update_plan.diagnostics.candidate_policies,
                update_plan.diagnostics.applicable_policies,
                update_plan.diagnostics.dominated_policies,
            );
        }
        let target_table = sink.clone().unwrap_or_default();
        apply_update_scope_plan(
            &update_plan,
            assignments,
            selection,
            &context,
            &self.store,
            &table_scope,
            &self.catalog,
            &target_table,
            &self.ui_followup,
        )?;
        Ok(())
    }
}
