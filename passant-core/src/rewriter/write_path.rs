use std::collections::HashSet;

use sqlparser::ast::{
    Assignment, Expr, Ident, MergeAction, Select, SelectItem, SetExpr, Statement, TableFactor,
    TableWithJoins,
};

use crate::identifiers::{AliasByBase, TableKey};
use crate::policy::{PolicyIr, Resolution};
use crate::source_sets::table_factor_source_tables;

use super::RewriteError;
use super::aggregates::{
    aggregate_temp_column, aggregate_temp_projection_expr, policy_aggregate_temp_entries,
};
use super::columns::{replace_sink_columns, rewrite_column_qualifiers};
use super::expr::{bool_literal, filter_table_factor, join_conjuncts, parse_expr};
use super::helpers::{insert_select_mapping, update_assignment_mapping, update_target_name};
use super::policy_expr::{
    apply_update_resolution, build_pgn_over_filter_expr, policy_applicability, scan_policy_expr,
};
use super::projection::select_is_aggregation;
use super::scope::TableScope;
use super::types::{PassantRewriter, PolicyApplicability, RewriteContext, SourceAggregate};

impl PassantRewriter {
    pub(crate) fn scan_aggregate_temp_columns(
        &self,
        table_scope: &TableScope,
    ) -> Result<Vec<(SourceAggregate, String)>, RewriteError> {
        let mut temp_columns = Vec::new();
        let mut seen = HashSet::new();
        for policy in &self.policies {
            let PolicyIr::CompatAggregate(policy) = policy else {
                continue;
            };
            if policy.sink.is_some() {
                continue;
            }
            if !policy.sources.iter().all(|source| {
                table_scope
                    .direct_base_tables
                    .contains(&TableKey::new(source))
            }) {
                continue;
            }
            for aggregate in policy_aggregate_temp_entries(
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
    ) -> Result<(), RewriteError> {
        self.rewrite_derived_table_factor(source)?;
        let source_tables = table_factor_source_tables(source);
        let mut filters = Vec::new();
        for policy in &self.policies {
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
            let context = RewriteContext::default();
            let mut expr = parse_expr(constraint)?;
            expr = scan_policy_expr(expr, policy.sources(), &context, &AliasByBase::default())?;
            filters.push(expr);
        }
        if !filters.is_empty() {
            filter_table_factor(source, join_conjuncts(filters))?;
        }
        for clause in clauses {
            if let MergeAction::Update { assignments } = &mut clause.action {
                for assignment in assignments {
                    self.rewrite_expr_subqueries(&mut assignment.value)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn rewrite_statement(&self, statement: &mut Statement) -> Result<(), RewriteError> {
        match statement {
            Statement::Query(query) => self.rewrite_query(query, None),
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
                };
                let before_columns = insert.columns.len();
                if let Some(source) = insert.source.as_mut() {
                    self.rewrite_query_with_context(source, &context)?;
                }
                self.apply_aggregate_insert_columns(insert, &sink, &context)?;
                if insert.columns.len() == before_columns {
                    self.append_invalidation_output_columns(insert, &sink);
                }
                Ok(())
            }
            Statement::Update {
                table,
                assignments,
                from,
                selection,
                ..
            } => self.rewrite_update(table, assignments, from.as_ref(), selection),
            Statement::Merge {
                table,
                source,
                on,
                clauses,
                ..
            } => self.rewrite_merge(table, source, on, clauses),
            Statement::Delete(_) if !self.policies.is_empty() => Err(
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

    fn append_invalidation_output_columns(&self, insert: &mut sqlparser::ast::Insert, sink: &str) {
        let has_invalidate = self.policies.iter().any(|policy| {
            policy
                .sink()
                .is_some_and(|policy_sink| policy_sink.eq_ignore_ascii_case(sink))
                && matches!(
                    policy.resolution(),
                    Resolution::Invalidate | Resolution::InvalidateMessage
                )
        });
        if !has_invalidate {
            return;
        }

        if insert.columns.is_empty() {
            return;
        }

        if self.policies.iter().any(|policy| {
            policy
                .sink()
                .is_some_and(|policy_sink| policy_sink.eq_ignore_ascii_case(sink))
                && policy.resolution() == Resolution::Invalidate
        }) && !insert
            .columns
            .iter()
            .any(|column| column.value.eq_ignore_ascii_case("valid"))
        {
            insert.columns.push(Ident::new("valid"));
        }

        if self.policies.iter().any(|policy| {
            policy
                .sink()
                .is_some_and(|policy_sink| policy_sink.eq_ignore_ascii_case(sink))
                && policy.resolution() == Resolution::InvalidateMessage
        }) && !insert
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
    ) -> Result<(), RewriteError> {
        let sink = update_target_name(table);
        let mut table_scope = TableScope::default();
        table_scope.add_table_with_joins(table);
        if let Some(from) = from {
            table_scope.add_table_with_joins(from);
        }
        let applicable = self
            .policies
            .iter()
            .filter_map(|policy| {
                policy_applicability(policy, &table_scope.base_tables, sink.as_deref(), false)
                    .map(|applicability| (policy, applicability))
            })
            .collect::<Vec<_>>();
        if applicable.is_empty() {
            return Ok(());
        }

        let context = RewriteContext {
            sink: sink.clone(),
            sink_expr_by_column: update_assignment_mapping(assignments),
            allow_partial_source_visibility: false,
        };
        for (policy, applicability) in applicable {
            match policy {
                PolicyIr::CompatDfc {
                    constraint,
                    on_fail,
                    sink_alias,
                    description,
                    ..
                } => {
                    let expr = if applicability == PolicyApplicability::RequiredSourceMissing {
                        bool_literal(false)
                    } else {
                        let mut expr = parse_expr(constraint)?;
                        if let Some(sink) = &context.sink {
                            expr = replace_sink_columns(expr, sink, &context.sink_expr_by_column);
                            expr = replace_sink_columns(
                                expr,
                                "_OUTPUT_",
                                &context.sink_expr_by_column,
                            );
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
                            &context,
                            &table_scope.alias_by_base,
                        )?
                    };
                    apply_update_resolution(
                        assignments,
                        selection,
                        expr,
                        *on_fail,
                        description.as_deref(),
                    )?;
                }
                PolicyIr::NativePgn(pgn) if pgn.kind == crate::policy::PgnPolicyKind::Update => {
                    let expr = if applicability == PolicyApplicability::RequiredSourceMissing {
                        bool_literal(false)
                    } else {
                        build_pgn_over_filter_expr(
                            &pgn.scope.sources,
                            &pgn.constraint,
                            &pgn.scope.sink_alias,
                            applicability,
                            &context,
                            &table_scope,
                        )?
                    };
                    apply_update_resolution(
                        assignments,
                        selection,
                        expr,
                        pgn.on_fail,
                        pgn.description.as_deref(),
                    )?;
                }
                _ => {}
            }
        }
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
        for (aggregate, temp_name) in
            self.source_aggregate_temp_columns(sink, Some(&table_scope))?
        {
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
