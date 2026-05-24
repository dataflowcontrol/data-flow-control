use std::collections::{HashMap, HashSet};

use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, FunctionArguments, JoinConstraint, JoinOperator, Query,
    Select, SelectItem, SetExpr, SetOperator, TableFactor, TableWithJoins,
};

use crate::identifiers::TableKey;
use crate::policy::{PolicyIr, Resolution};
use crate::source_sets::{
    cross_source_policies_for_branch, select_has_anti_join, select_has_full_join,
    select_nullable_source_tables, set_expr_source_tables,
    set_operation_requires_cross_source_policies, split_select_policies_for_nullable_joins,
    split_set_operation_policies,
};

use super::RewriteError;
use super::aggregates::transform_scan_aggregates;
use super::columns::rewrite_column_qualifiers;
use super::expr::{
    add_filter, and_expr, apply_resolution, filter_table_factor, join_conjuncts, parse_expr,
    table_factor_base_and_alias,
};
use super::helpers::{
    direct_source_occurrence_counts, prune_dominated_remove_policies, table_joins_all_inner,
    table_with_joins_base_tables,
};
use super::policy_expr::{
    build_compat_dfc_filter_expr, build_invalidate_projection_expr, build_pgn_over_filter_expr,
    join_pushdown_expr, join_pushdown_policy_matches, non_distributive_aggregates,
    policy_applicability, unique_column_guard_from_constraint,
};
use super::projection::select_is_aggregation;
use super::scope::TableScope;
use super::types::{PassantRewriter, RewriteContext};

impl PassantRewriter {
    pub(crate) fn rewrite_expr_subqueries(&self, expr: &mut Expr) -> Result<(), RewriteError> {
        match expr {
            Expr::Exists {
                subquery,
                negated: true,
            }
            | Expr::InSubquery {
                subquery,
                negated: true,
                ..
            } => self.rewrite_query(subquery, None),
            Expr::Exists { subquery, .. } | Expr::InSubquery { subquery, .. } => {
                self.rewrite_query(subquery, None)
            }
            Expr::Subquery(subquery) => self.rewrite_query(subquery, None),
            Expr::BinaryOp { left, right, .. } => {
                self.rewrite_expr_subqueries(left)?;
                self.rewrite_expr_subqueries(right)
            }
            Expr::Nested(expr)
            | Expr::UnaryOp { expr, .. }
            | Expr::IsFalse(expr)
            | Expr::IsNotFalse(expr)
            | Expr::IsTrue(expr)
            | Expr::IsNotTrue(expr)
            | Expr::IsNull(expr)
            | Expr::IsNotNull(expr) => self.rewrite_expr_subqueries(expr),
            Expr::Function(function) => self.rewrite_function_subqueries(function),
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => {
                if let Some(operand) = operand {
                    self.rewrite_expr_subqueries(operand)?;
                }
                for expr in conditions.iter_mut().chain(results.iter_mut()) {
                    self.rewrite_expr_subqueries(expr)?;
                }
                if let Some(else_result) = else_result {
                    self.rewrite_expr_subqueries(else_result)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn rewrite_function_subqueries(
        &self,
        function: &mut sqlparser::ast::Function,
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
                } => self.rewrite_expr_subqueries(expr)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn rewrite_set_operation_with_cross_source_fallback(
        &self,
        left: &mut SetExpr,
        right: &mut SetExpr,
        context: &RewriteContext,
    ) -> Result<(), RewriteError> {
        let left_tables = set_expr_source_tables(left);
        let right_tables = set_expr_source_tables(right);
        let left_policies = cross_source_policies_for_branch(&self.policies, &left_tables);
        let right_policies = cross_source_policies_for_branch(&self.policies, &right_tables);
        let mut branch_context = context.clone();
        branch_context.allow_partial_source_visibility = true;
        PassantRewriter {
            policies: left_policies,
            catalog: self.catalog.clone(),
        }
        .rewrite_set_expr(left, &branch_context)?;
        PassantRewriter {
            policies: right_policies,
            catalog: self.catalog.clone(),
        }
        .rewrite_set_expr(right, &branch_context)
    }

    fn apply_join_input_source_filters(
        &self,
        select: &mut Select,
    ) -> Result<HashSet<usize>, RewriteError> {
        if (!select_has_full_join(select) && !select_has_anti_join(select))
            || self.policies.is_empty()
        {
            return Ok(HashSet::new());
        }

        let occurrence_counts = direct_source_occurrence_counts(select);
        let mut pushed_counts: HashMap<usize, usize> = HashMap::new();
        for table in &mut select.from {
            let mut relation_filter = Vec::new();
            if (table.joins.iter().any(|join| {
                matches!(
                    join.join_operator,
                    JoinOperator::FullOuter(_) | JoinOperator::RightAnti(_)
                )
            })) && let Some((base, _)) = table_factor_base_and_alias(&table.relation)
            {
                for (index, policy) in self.policies.iter().enumerate() {
                    if join_pushdown_policy_matches(policy, &base) {
                        relation_filter.push(join_pushdown_expr(
                            policy,
                            &base,
                            None,
                            &self.catalog,
                        )?);
                        *pushed_counts.entry(index).or_default() += 1;
                    }
                }
            }
            if !relation_filter.is_empty() {
                filter_table_factor(&mut table.relation, join_conjuncts(relation_filter))?;
            }

            for join in &mut table.joins {
                let mut join_filter = Vec::new();
                if matches!(
                    join.join_operator,
                    JoinOperator::FullOuter(_) | JoinOperator::Anti(_) | JoinOperator::LeftAnti(_)
                ) && let Some((base, _)) = table_factor_base_and_alias(&join.relation)
                {
                    for (index, policy) in self.policies.iter().enumerate() {
                        if join_pushdown_policy_matches(policy, &base) {
                            join_filter.push(join_pushdown_expr(
                                policy,
                                &base,
                                None,
                                &self.catalog,
                            )?);
                            *pushed_counts.entry(index).or_default() += 1;
                        }
                    }
                }
                if !join_filter.is_empty() {
                    filter_table_factor(&mut join.relation, join_conjuncts(join_filter))?;
                }
            }
        }

        let mut pushed = HashSet::new();
        for (index, count) in pushed_counts {
            let Some(policy) = self.policies.get(index) else {
                continue;
            };
            let Some(source) = policy.sources().first() else {
                continue;
            };
            if occurrence_counts
                .get(&source.to_ascii_lowercase())
                .is_some_and(|occurrences| count >= *occurrences)
            {
                pushed.insert(index);
            }
        }
        Ok(pushed)
    }

    fn rewrite_select(
        &self,
        select: &mut Select,
        context: &RewriteContext,
    ) -> Result<(), RewriteError> {
        let exists_handled = self.rewrite_exists_subqueries_as_joins_impl(select)?;
        self.rewrite_expression_subqueries(select)?;
        self.rewrite_derived_subqueries(select)?;
        if let Some(policies) = split_select_policies_for_nullable_joins(
            &self.policies,
            select,
            &TableScope::from_select(select).direct_base_tables,
        ) {
            return PassantRewriter {
                policies,
                catalog: self.catalog.clone(),
            }
            .rewrite_select(select, context);
        }
        let mut pushed_policy_indices = self.apply_join_input_source_filters(select)?;
        pushed_policy_indices.extend(self.apply_join_policy_pushdown(select)?);
        let table_scope = TableScope::from_select(select);
        let nullable_sources = select_nullable_source_tables(select);
        let applicable = self
            .policies
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                !pushed_policy_indices.contains(index) && !exists_handled.contains(index)
            })
            .filter_map(|(_, policy)| {
                policy_applicability(
                    policy,
                    &table_scope.direct_base_tables,
                    context.sink.as_deref(),
                    context.allow_partial_source_visibility,
                )
                .map(|applicability| (policy, applicability))
            })
            .collect::<Vec<_>>();
        if applicable.is_empty() {
            return Ok(());
        }
        let applicable = prune_dominated_remove_policies(applicable);
        let _ = (&nullable_sources, &applicable);

        let is_aggregation = select_is_aggregation(select);
        for (policy, applicability) in applicable {
            match policy {
                PolicyIr::CompatDfc {
                    sources,
                    constraint,
                    on_fail,
                    sink_alias,
                    description,
                    ..
                } => {
                    let mut expr = build_compat_dfc_filter_expr(
                        sources,
                        constraint,
                        sink_alias,
                        applicability,
                        context,
                        &table_scope,
                        is_aggregation,
                    )?;
                    if let Some(guard) =
                        unique_column_guard_from_constraint(constraint, &self.catalog)
                    {
                        expr = and_expr(guard, expr);
                    }
                    let projection_expr = if matches!(on_fail, Resolution::Invalidate)
                        && context.sink.is_some()
                        && sources.len() > 1
                    {
                        build_invalidate_projection_expr(
                            sources,
                            constraint,
                            sink_alias,
                            applicability,
                            context,
                            &table_scope,
                        )?
                    } else {
                        expr.clone()
                    };
                    apply_resolution(
                        select,
                        expr,
                        *on_fail,
                        description.as_deref(),
                        is_aggregation,
                        Some(projection_expr),
                    )?;
                }
                PolicyIr::CompatAggregate(_) => {}
                PolicyIr::NativePgn(pgn) if pgn.kind == crate::policy::PgnPolicyKind::Over => {
                    let expr = build_pgn_over_filter_expr(
                        &pgn.scope.sources,
                        &pgn.constraint,
                        &pgn.scope.sink_alias,
                        applicability,
                        context,
                        &table_scope,
                    )?;
                    apply_resolution(
                        select,
                        expr,
                        pgn.on_fail,
                        pgn.description.as_deref(),
                        is_aggregation,
                        None,
                    )?;
                }
                PolicyIr::NativePgn(_) => {}
            }
        }
        if context.sink.is_none() {
            self.apply_aggregate_scan_columns(select)?;
        }
        Ok(())
    }

    fn rewrite_expression_subqueries(&self, select: &mut Select) -> Result<(), RewriteError> {
        for item in &mut select.projection {
            match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    self.rewrite_expr_subqueries(expr)?;
                }
                _ => {}
            }
        }
        if let Some(selection) = &mut select.selection {
            self.rewrite_expr_subqueries(selection)?;
        }
        if let Some(having) = &mut select.having {
            self.rewrite_expr_subqueries(having)?;
        }
        Ok(())
    }

    pub(crate) fn rewrite_derived_table_factor(
        &self,
        factor: &mut TableFactor,
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
            self.rewrite_query(subquery, None)?;
        }
        Ok(())
    }

    fn rewrite_derived_table_with_joins(
        &self,
        table: &mut TableWithJoins,
    ) -> Result<(), RewriteError> {
        self.rewrite_derived_table_factor(&mut table.relation)?;
        for join in &mut table.joins {
            self.rewrite_derived_table_factor(&mut join.relation)?;
        }
        Ok(())
    }

    fn rewrite_derived_subqueries(&self, select: &mut Select) -> Result<(), RewriteError> {
        for table in &mut select.from {
            self.rewrite_derived_table_with_joins(table)?;
        }
        Ok(())
    }

    pub(crate) fn rewrite_query(
        &self,
        query: &mut Query,
        sink: Option<&str>,
    ) -> Result<(), RewriteError> {
        let context = RewriteContext {
            sink: sink.map(str::to_string),
            sink_expr_by_column: HashMap::new(),
            allow_partial_source_visibility: false,
        };
        self.rewrite_query_with_context(query, &context)
    }

    fn apply_join_policy_pushdown(
        &self,
        select: &mut Select,
    ) -> Result<HashSet<usize>, RewriteError> {
        let occurrence_counts = direct_source_occurrence_counts(select);
        let mut pushed_counts: HashMap<usize, usize> = HashMap::new();
        let mut selection_filters = Vec::new();
        let mut pushed = HashSet::new();
        for table in &mut select.from {
            let left_base_and_alias = table_factor_base_and_alias(&table.relation);
            if table_joins_all_inner(table)
                && let Some((base, alias)) = &left_base_and_alias
            {
                for (index, policy) in self.policies.iter().enumerate() {
                    if !join_pushdown_policy_matches(policy, base) {
                        continue;
                    }
                    let expr = join_pushdown_expr(policy, base, alias.clone(), &self.catalog)?;
                    selection_filters.push(expr);
                    *pushed_counts.entry(index).or_default() += 1;
                }
            }
            for join in &mut table.joins {
                let target = match &mut join.join_operator {
                    JoinOperator::Inner(JoinConstraint::On(existing_on)) => {
                        table_factor_base_and_alias(&join.relation)
                            .map(|base_and_alias| (existing_on, base_and_alias))
                    }
                    JoinOperator::LeftOuter(JoinConstraint::On(existing_on)) => {
                        table_factor_base_and_alias(&join.relation)
                            .map(|base_and_alias| (existing_on, base_and_alias))
                    }
                    JoinOperator::RightOuter(JoinConstraint::On(existing_on)) => {
                        left_base_and_alias
                            .clone()
                            .map(|base_and_alias| (existing_on, base_and_alias))
                    }
                    JoinOperator::Semi(JoinConstraint::On(existing_on))
                    | JoinOperator::LeftSemi(JoinConstraint::On(existing_on)) => {
                        table_factor_base_and_alias(&join.relation)
                            .map(|base_and_alias| (existing_on, base_and_alias))
                    }
                    JoinOperator::RightSemi(JoinConstraint::On(existing_on)) => left_base_and_alias
                        .clone()
                        .map(|base_and_alias| (existing_on, base_and_alias)),
                    JoinOperator::FullOuter(_) if !self.policies.is_empty() => None,
                    _ => None,
                };
                let Some((existing_on, (base, alias))) = target else {
                    continue;
                };

                for (index, policy) in self.policies.iter().enumerate() {
                    if !join_pushdown_policy_matches(policy, &base) {
                        continue;
                    }
                    let expr = join_pushdown_expr(policy, &base, alias.clone(), &self.catalog)?;
                    *existing_on = and_expr(existing_on.clone(), expr);
                    *pushed_counts.entry(index).or_default() += 1;
                }
            }
        }
        for (index, policy) in self.policies.iter().enumerate() {
            if pushed.contains(&index) {
                continue;
            }
            let PolicyIr::CompatDfc {
                sources,
                constraint,
                on_fail: Resolution::Remove,
                required_sources,
                sink,
                ..
            } = policy
            else {
                continue;
            };
            if !required_sources.is_empty() || sink.is_some() || sources.len() < 2 {
                continue;
            }
            let expr = parse_expr(constraint)?;
            if !non_distributive_aggregates(&expr)?.is_empty() {
                continue;
            }
            let table_scope = TableScope::from_select(select);
            for table in &mut select.from {
                if !table_joins_all_inner(table) {
                    continue;
                }
                let bases = table_with_joins_base_tables(table);
                if !sources
                    .iter()
                    .all(|source| bases.contains(&TableKey::new(source)))
                {
                    continue;
                }
                let mut transformed = transform_scan_aggregates(expr.clone())?;
                rewrite_column_qualifiers(&mut transformed, &table_scope.alias_by_base);
                if let Some(join) = table.joins.last_mut()
                    && let JoinOperator::Inner(JoinConstraint::On(existing_on)) =
                        &mut join.join_operator
                {
                    *existing_on = and_expr(existing_on.clone(), transformed);
                    pushed.insert(index);
                    break;
                }
            }
        }
        for expr in selection_filters {
            add_filter(select, expr, false)?;
        }
        for (index, count) in pushed_counts {
            let Some(policy) = self.policies.get(index) else {
                continue;
            };
            let Some(source) = policy.sources().first() else {
                continue;
            };
            if occurrence_counts
                .get(&source.to_ascii_lowercase())
                .is_some_and(|occurrences| count >= *occurrences)
            {
                pushed.insert(index);
            }
        }
        Ok(pushed)
    }

    pub(crate) fn rewrite_set_expr(
        &self,
        set_expr: &mut SetExpr,
        context: &RewriteContext,
    ) -> Result<(), RewriteError> {
        match set_expr {
            SetExpr::Select(select) => self.rewrite_select(select, context),
            SetExpr::Query(query) => self.rewrite_query_with_context(query, context),
            SetExpr::SetOperation {
                op, left, right, ..
            } => {
                if set_operation_requires_cross_source_policies(&self.policies, left, right) {
                    let Some((left_policies, right_policies)) =
                        split_set_operation_policies(&self.policies, left, right)
                    else {
                        if matches!(op, SetOperator::Except) {
                            self.rewrite_set_operation_with_cross_source_fallback(
                                left, right, context,
                            )?;
                        }
                        return Ok(());
                    };
                    PassantRewriter {
                        policies: left_policies,
                        catalog: self.catalog.clone(),
                    }
                    .rewrite_set_expr(left, context)?;
                    PassantRewriter {
                        policies: right_policies,
                        catalog: self.catalog.clone(),
                    }
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
        context: &RewriteContext,
    ) -> Result<(), RewriteError> {
        if let Some(with) = query.with.as_mut() {
            for cte in &mut with.cte_tables {
                self.rewrite_query(&mut cte.query, None)?;
            }
        }
        self.rewrite_set_expr(query.body.as_mut(), context)
    }
}
