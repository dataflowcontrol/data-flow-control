use std::collections::HashMap;
use std::collections::HashSet;

use sqlparser::ast::{
    Assignment, Expr, Ident, MergeAction, Select, SelectItem, SetExpr, Statement, TableFactor,
    TableWithJoins,
};

use crate::policy::PolicyIr;
use crate::source_sets::table_factor_source_tables;

use super::RewriteError;
use super::aggregates::{
    aggregate_temp_column, aggregate_temp_projection_expr, policy_aggregate_temp_entries_from_expr,
};
use super::expr::{filter_table_factor, join_conjuncts, parse_expr};
use super::helpers::{insert_select_mapping, update_assignment_mapping, update_target_name};
use super::plan::{
    apply_update_scope_plan, plan_insert_aggregate_temp_columns, plan_insert_sink_invalidation,
    plan_merge_source_filters, plan_update_scope,
};
use super::projection::select_is_aggregation;
use super::scope::TableScope;
use super::types::{PassantRewriter, RewriteContext, SourceAggregate};

impl PassantRewriter {
    pub(crate) fn scan_aggregate_temp_columns(
        &self,
        table_scope: &TableScope,
    ) -> Result<Vec<(SourceAggregate, String)>, RewriteError> {
        let mut temp_columns = Vec::new();
        let mut seen = HashSet::new();
        for index in self
            .store
            .aggregate_scan_policy_lookup(&table_scope.direct_base_tables)
            .iter()
        {
            let Some(PolicyIr::CompatAggregate(policy)) = self.store.policy(index) else {
                continue;
            };
            let constraint_expr = self
                .store
                .constraint_expr(index, &policy.constraint, None)?;
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
        Ok(temp_columns
            .into_iter()
            .enumerate()
            .map(|(index, (aggregate, _))| (aggregate, aggregate_temp_column(index + 1)))
            .collect())
    }

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
                    sink: Some(sink.clone()),
                    sink_expr_by_column: insert_select_mapping(insert),
                    allow_partial_source_visibility: false,
                    collect_stats,
                };
                let before_columns = insert.columns.len();
                if let Some(source) = insert.source.as_mut() {
                    self.rewrite_query_with_context(source, &context)?;
                }
                self.apply_aggregate_insert_columns(insert, &sink, &context)?;
                if insert.columns.len() == before_columns {
                    self.append_invalidation_output_columns(insert, &sink, collect_stats);
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
            .map(|column| Ident::new(column.as_str()))
            .collect();
    }

    fn append_invalidation_output_columns(
        &self,
        insert: &mut sqlparser::ast::Insert,
        sink: &str,
        collect_stats: bool,
    ) {
        let invalidation_plan = plan_insert_sink_invalidation(&self.store, sink);
        self.statement_summary
            .record_scope(invalidation_plan.diagnostics.clone());
        if collect_stats {
            self.stats.accumulate_scope_diagnostics(
                invalidation_plan.diagnostics.candidate_policies,
                invalidation_plan.diagnostics.applicable_policies,
                0,
            );
        }
        if !invalidation_plan.append_valid && !invalidation_plan.append_invalid_string {
            return;
        }

        if insert.columns.is_empty() {
            return;
        }

        if invalidation_plan.append_valid
            && !insert
                .columns
                .iter()
                .any(|column| column.value.eq_ignore_ascii_case("valid"))
        {
            insert.columns.push(Ident::new("valid"));
        }

        if invalidation_plan.append_invalid_string
            && !insert
                .columns
                .iter()
                .any(|column| column.value.eq_ignore_ascii_case("invalid_string"))
        {
            insert.columns.push(Ident::new("invalid_string"));
        }
    }

    pub(crate) fn apply_aggregate_scan_columns(
        &self,
        select: &mut Select,
    ) -> Result<(), RewriteError> {
        let table_scope = TableScope::from_select(select);
        for (aggregate, temp_name) in self.scan_aggregate_temp_columns(&table_scope)? {
            select.projection.push(SelectItem::ExprWithAlias {
                expr: parse_expr(&aggregate.sql)?,
                alias: Ident::new(&temp_name),
            });
        }
        Ok(())
    }

    fn rewrite_update(
        &self,
        table: &TableWithJoins,
        assignments: &mut Vec<Assignment>,
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

    fn apply_aggregate_insert_columns(
        &self,
        insert: &mut sqlparser::ast::Insert,
        sink: &str,
        context: &RewriteContext,
    ) -> Result<(), RewriteError> {
        let source = insert.source.as_mut();
        let Some(query) = source else {
            return Ok(());
        };
        let SetExpr::Select(select) = query.body.as_mut() else {
            return Ok(());
        };
        let table_scope = TableScope::from_select(select);
        let is_query_aggregation = select_is_aggregation(select);
        let stats = context.collect_stats.then_some(&self.stats);
        let column_plan =
            plan_insert_aggregate_temp_columns(&self.store, stats, sink, &table_scope)?;
        self.statement_summary
            .record_scope(column_plan.diagnostics.clone());
        if context.collect_stats {
            self.stats.accumulate_scope_diagnostics(
                column_plan.diagnostics.candidate_policies,
                column_plan.diagnostics.applicable_policies,
                0,
            );
        }
        for (aggregate, temp_name) in column_plan.temp_columns {
            let expr = aggregate_temp_projection_expr(
                &aggregate,
                is_query_aggregation,
                Some(context),
                Some(sink),
                Some(select),
            )?;
            select.projection.push(SelectItem::ExprWithAlias {
                expr,
                alias: Ident::new(&temp_name),
            });
            if !insert.columns.is_empty()
                && !insert
                    .columns
                    .iter()
                    .any(|column| column.value.eq_ignore_ascii_case(&temp_name))
            {
                insert.columns.push(Ident::new(&temp_name));
            }
        }
        Ok(())
    }
}
