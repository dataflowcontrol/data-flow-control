use std::collections::HashMap;

use sqlparser::ast::{
    Assignment, Expr, MergeAction, SetExpr, Statement, TableFactor, TableWithJoins,
};

use crate::source_sets::table_factor_source_tables;

use super::RewriteError;
use super::expr::{filter_table_factor, join_conjuncts};
use super::helpers::{insert_select_mapping, update_assignment_mapping, update_target_name};
use super::plan::{apply_update_scope_plan, plan_merge_source_filters, plan_update_scope};
use super::scope::TableScope;
use super::types::{PassantRewriter, RewriteContext};

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
        collect_stats: bool,
    ) -> Result<(), RewriteError> {
        match statement {
            Statement::Query(query) => {
                let context = RewriteContext {
                    sink: None,
                    sink_expr_by_column: HashMap::new(),
                    allow_partial_source_visibility: false,
                    collect_stats,
                };
                self.rewrite_query_with_context(query, &context)
            }
            Statement::Insert(insert) => {
                let sink = insert.table_name.to_string();
                if insert.source.is_none() {
                    return Ok(());
                }
                self.expand_insert_columns_from_catalog(insert, &sink);
                let context = RewriteContext {
                    sink: Some(sink),
                    sink_expr_by_column: insert_select_mapping(insert),
                    allow_partial_source_visibility: false,
                    collect_stats,
                };
                if let Some(source) = insert.source.as_mut() {
                    self.rewrite_query_with_context(source, &context)?;
                }
                Ok(())
            }
            Statement::Update {
                table,
                assignments,
                from,
                selection,
                ..
            } => self.rewrite_update(table, assignments, from.as_ref(), selection, collect_stats),
            Statement::Merge {
                table,
                source,
                on,
                clauses,
                ..
            } => self.rewrite_merge(table, source, on, clauses, collect_stats),
            Statement::Delete(_) if !self.store.is_empty() => Err(
                RewriteError::unsupported_statement("delete with registered policies"),
            ),
            _ => Ok(()),
        }
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
        collect_stats: bool,
    ) -> Result<(), RewriteError> {
        let sink = update_target_name(table);
        let mut table_scope = TableScope::default();
        table_scope.add_table_with_joins(table);
        if let Some(from) = from {
            table_scope.add_table_with_joins(from);
        }
        let context = RewriteContext {
            sink: sink.clone(),
            sink_expr_by_column: update_assignment_mapping(assignments),
            allow_partial_source_visibility: false,
            collect_stats,
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
        apply_update_scope_plan(&update_plan, assignments, selection)?;
        Ok(())
    }
}
