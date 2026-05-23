use std::collections::{HashMap, HashSet};

use sqlparser::ast::{
    Assignment, AssignmentTarget, BinaryOperator, Expr, FunctionArg, FunctionArgExpr,
    FunctionArguments, Ident, JoinConstraint, JoinOperator, ObjectName, Query, Select, SelectItem,
    SetExpr, SetOperator, Statement, TableAlias, TableFactor, TableWithJoins, Value,
};
use thiserror::Error;

use crate::parser::{ParseError, parse_query};
use crate::policy::{PolicyIr, PolicyParseError, Resolution, parse_policy_text};
use crate::semiring;
use crate::threshold;

#[derive(Debug, Error)]
pub enum RewriteError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error(transparent)]
    PolicyParse(#[from] PolicyParseError),
    #[error("unsupported query form: {0}")]
    Unsupported(String),
    #[error("unsupported resolution for SQL-only rewrite: {0:?}")]
    UnsupportedResolution(Resolution),
}

#[derive(Debug, Default, Clone)]
pub struct PassantRewriter {
    policies: Vec<PolicyIr>,
}

#[derive(Debug, Clone)]
pub struct FinalizeQuery {
    pub policy_id: String,
    pub sql: String,
    pub invalidate_sql: Option<String>,
    pub description: Option<String>,
    pub constraint: String,
}

#[derive(Debug, Clone)]
struct SourceAggregate {
    sql: String,
    function_name: String,
    expr: Expr,
    input: Expr,
}

#[derive(Debug, Clone, Default)]
struct RewriteContext {
    sink: Option<String>,
    sink_expr_by_column: HashMap<String, Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PolicyApplicability {
    Normal,
    RequiredSourceMissing,
}

impl PassantRewriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_policy(&mut self, policy: PolicyIr) {
        self.policies.push(policy);
    }

    pub fn register_policy_text(&mut self, text: &str) -> Result<(), RewriteError> {
        self.register_policy(parse_policy_text(text)?);
        Ok(())
    }

    pub fn delete_policy(
        &mut self,
        sources: Option<&[String]>,
        sink: Option<&str>,
        constraint: Option<&str>,
        on_fail: Option<Resolution>,
        description: Option<&str>,
    ) -> bool {
        let Some(index) = self.policies.iter().position(|policy| {
            if let Some(sources) = sources
                && policy.sources() != sources
            {
                return false;
            }
            if let Some(sink) = sink
                && policy.sink() != Some(sink)
            {
                return false;
            }
            if let Some(constraint) = constraint
                && policy.constraint() != constraint
            {
                return false;
            }
            if let Some(on_fail) = on_fail
                && policy.resolution() != on_fail
            {
                return false;
            }
            if let Some(description) = description
                && policy_description(policy) != Some(description)
            {
                return false;
            }
            true
        }) else {
            return false;
        };
        self.policies.remove(index);
        true
    }

    pub fn policies(&self) -> &[PolicyIr] {
        &self.policies
    }

    pub fn dfc_policies(&self) -> Vec<PolicyIr> {
        self.policies
            .iter()
            .filter(|policy| matches!(policy, PolicyIr::CompatDfc { .. }))
            .cloned()
            .collect()
    }

    pub fn aggregate_policies(&self) -> Vec<PolicyIr> {
        self.policies
            .iter()
            .filter(|policy| matches!(policy, PolicyIr::CompatAggregate(_)))
            .cloned()
            .collect()
    }

    pub fn rewrite(&self, sql: &str) -> Result<String, RewriteError> {
        let mut statement = parse_query(sql)?;
        if let Some(sql) = self.rewrite_limited_query(&statement)? {
            return Ok(sql);
        }
        self.rewrite_statement(&mut statement)?;
        Ok(statement.to_string())
    }

    fn rewrite_limited_query(&self, statement: &Statement) -> Result<Option<String>, RewriteError> {
        let Statement::Query(query) = statement else {
            return Ok(None);
        };
        if query.limit.is_none() && query.offset.is_none() && query.fetch.is_none() {
            return Ok(None);
        }
        let SetExpr::Select(select) = query.body.as_ref() else {
            return Ok(None);
        };
        let table_scope = TableScope::from_select(select);
        let mut filters = Vec::new();
        let mut propagated_filter_columns = HashMap::new();
        let projected_names = projected_select_names(select);
        for (policy, applicability) in self.policies.iter().filter_map(|policy| {
            policy_applicability(policy, &table_scope.direct_base_tables, None)
                .map(|applicability| (policy, applicability))
        }) {
            let PolicyIr::CompatDfc {
                constraint,
                on_fail,
                ..
            } = policy
            else {
                continue;
            };
            if !matches!(
                on_fail,
                Resolution::Remove | Resolution::Kill | Resolution::Llm
            ) {
                continue;
            }
            let mut expr = if applicability == PolicyApplicability::RequiredSourceMissing {
                bool_literal(false)
            } else {
                let mut expr = parse_expr(constraint)?;
                rewrite_column_qualifiers(&mut expr, &table_scope.alias_by_base);
                expr = scan_policy_expr(
                    expr,
                    policy.sources(),
                    &RewriteContext::default(),
                    &table_scope.alias_by_base,
                )?;
                let mut source_columns = HashMap::new();
                collect_compound_columns_by_name(&expr, &mut source_columns);
                unqualify_columns(&mut expr);
                for (name, source_expr) in source_columns {
                    if !projected_names.contains(&name) {
                        let alias = format!("__passant_filter_{name}");
                        propagated_filter_columns
                            .entry(name)
                            .or_insert((source_expr, alias));
                    }
                }
                expr
            };
            if *on_fail == Resolution::Kill {
                expr = kill_expr(expr)?;
            } else if *on_fail == Resolution::Llm {
                expr = resolver_expr(expr)?;
            }
            let replacements = propagated_filter_columns
                .iter()
                .map(|(name, (_, alias))| (name.clone(), alias.clone()))
                .collect::<HashMap<_, _>>();
            replace_identifiers(&mut expr, &replacements);
            filters.push(expr.to_string());
        }
        if filters.is_empty() {
            return Ok(None);
        }
        let mut inner_statement = statement.clone();
        if let Statement::Query(inner_query) = &mut inner_statement
            && let SetExpr::Select(inner_select) = inner_query.body.as_mut()
        {
            for (_, (expr, alias)) in propagated_filter_columns {
                inner_select.projection.push(SelectItem::ExprWithAlias {
                    expr,
                    alias: Ident::new(alias),
                });
            }
        }
        let outer_projection = outer_limited_projection(select);
        Ok(Some(format!(
            "SELECT {outer_projection} FROM ({inner_statement}) AS __passant_limited WHERE {}",
            filters.join(" AND ")
        )))
    }

    pub fn finalize_aggregate_policies(&self, sink_table: &str) -> Vec<(String, Option<String>)> {
        self.policies
            .iter()
            .filter_map(|policy| match policy {
                PolicyIr::CompatAggregate(policy)
                    if policy
                        .sink
                        .as_deref()
                        .is_none_or(|sink| sink.eq_ignore_ascii_case(sink_table)) =>
                {
                    Some((format!("aggregate::{}", policy.constraint), None))
                }
                _ => None,
            })
            .collect()
    }

    pub fn finalize_aggregate_queries(&self, sink_table: &str) -> Vec<FinalizeQuery> {
        let aggregate_temp_columns = self
            .source_aggregate_temp_columns(sink_table, None)
            .unwrap_or_default();
        self.policies
            .iter()
            .filter_map(|policy| match policy {
                PolicyIr::CompatAggregate(policy)
                    if policy
                        .sink
                        .as_deref()
                        .is_none_or(|sink| sink.eq_ignore_ascii_case(sink_table)) =>
                {
                    let constraint = rewrite_source_aggregates_for_finalize(
                        &policy.constraint,
                        &policy.sources,
                        &aggregate_temp_columns,
                    )
                    .unwrap_or_else(|_| policy.constraint.clone());
                    let (sql, invalidate_sql) =
                        aggregate_finalize_sql(sink_table, &constraint, &policy.dimensions);
                    Some(FinalizeQuery {
                        policy_id: format!("aggregate::{}", policy.constraint),
                        sql,
                        invalidate_sql: Some(invalidate_sql),
                        description: policy.description.clone(),
                        constraint: policy.constraint.clone(),
                    })
                }
                _ => None,
            })
            .collect()
    }

    fn rewrite_statement(&self, statement: &mut Statement) -> Result<(), RewriteError> {
        match statement {
            Statement::Query(query) => self.rewrite_query(query, None),
            Statement::Insert(insert) => {
                let sink = insert.table_name.to_string();
                let context = RewriteContext {
                    sink: Some(sink.clone()),
                    sink_expr_by_column: insert_select_mapping(insert),
                };
                let Some(source) = insert.source.as_mut() else {
                    return Ok(());
                };
                let before_columns = insert.columns.len();
                self.rewrite_query_with_context(source, &context)?;
                self.apply_aggregate_insert_columns(insert, &sink)?;
                if insert.columns.len() == before_columns {
                    self.expand_insert_columns_for_invalidations(insert, &sink);
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
            Statement::Delete(_) if !self.policies.is_empty() => Err(RewriteError::Unsupported(
                "delete with registered policies".into(),
            )),
            _ => Ok(()),
        }
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
                policy_applicability(policy, &table_scope.base_tables, sink.as_deref())
                    .map(|applicability| (policy, applicability))
            })
            .collect::<Vec<_>>();
        if applicable.is_empty() {
            return Ok(());
        }

        let context = RewriteContext {
            sink: sink.clone(),
            sink_expr_by_column: update_assignment_mapping(assignments),
        };
        for (policy, applicability) in applicable {
            let PolicyIr::CompatDfc {
                constraint,
                on_fail,
                sink_alias,
                description,
                ..
            } = policy
            else {
                continue;
            };
            let expr = if applicability == PolicyApplicability::RequiredSourceMissing {
                bool_literal(false)
            } else {
                let mut expr = parse_expr(constraint)?;
                if let Some(sink) = &context.sink {
                    expr = replace_sink_columns(expr, sink, &context.sink_expr_by_column);
                    expr = replace_sink_columns(expr, "_OUTPUT_", &context.sink_expr_by_column);
                    if let Some(sink_alias) = sink_alias {
                        expr = replace_sink_columns(expr, sink_alias, &context.sink_expr_by_column);
                    }
                }
                rewrite_column_qualifiers(&mut expr, &table_scope.alias_by_base);
                scan_policy_expr(expr, policy.sources(), &context, &table_scope.alias_by_base)?
            };
            match on_fail {
                Resolution::Remove => {
                    *selection = Some(match selection.take() {
                        Some(existing) => and_expr(existing, expr),
                        None => expr,
                    });
                }
                Resolution::Kill => {
                    let expr = kill_expr(expr)?;
                    *selection = Some(match selection.take() {
                        Some(existing) => and_expr(existing, expr),
                        None => expr,
                    });
                }
                Resolution::Invalidate => {
                    upsert_valid_assignment(assignments, expr);
                }
                Resolution::InvalidateMessage => {
                    upsert_invalid_string_assignment(assignments, expr, description.as_deref())?;
                }
                Resolution::Llm => {
                    let expr = resolver_expr(expr)?;
                    *selection = Some(match selection.take() {
                        Some(existing) => and_expr(existing, expr),
                        None => expr,
                    });
                }
            }
        }
        Ok(())
    }

    fn rewrite_query(&self, query: &mut Query, sink: Option<&str>) -> Result<(), RewriteError> {
        let context = RewriteContext {
            sink: sink.map(str::to_string),
            sink_expr_by_column: HashMap::new(),
        };
        self.rewrite_query_with_context(query, &context)
    }

    fn rewrite_query_with_context(
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

    fn rewrite_set_expr(
        &self,
        set_expr: &mut SetExpr,
        context: &RewriteContext,
    ) -> Result<(), RewriteError> {
        match set_expr {
            SetExpr::Select(select) => self.rewrite_select(select, context),
            SetExpr::Query(query) => self.rewrite_query_with_context(query, context),
            SetExpr::SetOperation {
                op: SetOperator::Except,
                ..
            } if !self.policies.is_empty() => Err(RewriteError::Unsupported(
                "EXCEPT with registered policies is non-monotonic".into(),
            )),
            SetExpr::SetOperation { left, right, .. } => {
                if self.set_operation_requires_source_sets(left, right) {
                    let Some((left_policies, right_policies)) =
                        self.split_set_operation_policies(left, right)
                    else {
                        return Err(RewriteError::Unsupported(
                            "set operation policy enforcement requires source-set annotations"
                                .into(),
                        ));
                    };
                    PassantRewriter {
                        policies: left_policies,
                    }
                    .rewrite_set_expr(left, context)?;
                    PassantRewriter {
                        policies: right_policies,
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

    fn rewrite_select(
        &self,
        select: &mut Select,
        context: &RewriteContext,
    ) -> Result<(), RewriteError> {
        self.rewrite_expression_subqueries(select)?;
        self.rewrite_derived_subqueries(select)?;
        if let Some(policies) = self.split_select_policies_for_source_sets(select) {
            return PassantRewriter { policies }.rewrite_select(select, context);
        }
        let mut pushed_policy_indices = self.apply_join_input_source_filters(select)?;
        pushed_policy_indices.extend(self.apply_join_policy_pushdown(select)?);
        let table_scope = TableScope::from_select(select);
        let nullable_sources = select_nullable_source_tables(select);
        let applicable = self
            .policies
            .iter()
            .enumerate()
            .filter(|(index, _)| !pushed_policy_indices.contains(index))
            .filter_map(|(_, policy)| {
                policy_applicability(
                    policy,
                    &table_scope.direct_base_tables,
                    context.sink.as_deref(),
                )
                .map(|applicability| (policy, applicability))
            })
            .collect::<Vec<_>>();
        if applicable.is_empty() {
            return Ok(());
        }
        let applicable = prune_dominated_remove_policies(applicable);
        reject_policies_requiring_source_sets(&applicable, &nullable_sources)?;
        reject_policies_on_anti_probe_sources(select, &applicable)?;

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
                        if !is_aggregation {
                            expr = scan_policy_expr(
                                expr,
                                sources,
                                context,
                                &table_scope.alias_by_base,
                            )?;
                        }
                        expr
                    };
                    apply_resolution(
                        select,
                        expr,
                        *on_fail,
                        description.as_deref(),
                        is_aggregation,
                    )?;
                }
                PolicyIr::CompatAggregate(_) | PolicyIr::NativeFlowGuard(_) => {}
            }
        }
        Ok(())
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
                    let expr = join_pushdown_expr(policy, base, alias.clone())?;
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
                    let expr = join_pushdown_expr(policy, &base, alias.clone())?;
                    *existing_on = and_expr(existing_on.clone(), expr);
                    *pushed_counts.entry(index).or_default() += 1;
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
                        relation_filter.push(join_pushdown_expr(policy, &base, None)?);
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
                            join_filter.push(join_pushdown_expr(policy, &base, None)?);
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

    fn set_operation_requires_source_sets(&self, left: &SetExpr, right: &SetExpr) -> bool {
        let left_tables = set_expr_source_tables(left);
        let right_tables = set_expr_source_tables(right);
        if left_tables.is_empty() || right_tables.is_empty() {
            return false;
        }
        let all_tables = left_tables
            .union(&right_tables)
            .cloned()
            .collect::<HashSet<_>>();

        self.policies.iter().any(|policy| {
            let sources = policy
                .sources()
                .iter()
                .map(|source| source.to_ascii_lowercase())
                .collect::<HashSet<_>>();
            sources.len() > 1
                && sources.iter().all(|source| all_tables.contains(source))
                && (!sources.iter().all(|source| left_tables.contains(source))
                    || !sources.iter().all(|source| right_tables.contains(source)))
        })
    }

    fn split_set_operation_policies(
        &self,
        left: &SetExpr,
        right: &SetExpr,
    ) -> Option<(Vec<PolicyIr>, Vec<PolicyIr>)> {
        let left_tables = set_expr_source_tables(left);
        let right_tables = set_expr_source_tables(right);
        let mut left_policies = Vec::new();
        let mut right_policies = Vec::new();

        for policy in &self.policies {
            if !policy_requires_set_split(policy, &left_tables, &right_tables) {
                left_policies.push(policy.clone());
                right_policies.push(policy.clone());
                continue;
            }
            let (left_split, right_split) =
                split_policy_for_set_branches(policy, &left_tables, &right_tables)?;
            left_policies.extend(left_split);
            right_policies.extend(right_split);
        }

        Some((left_policies, right_policies))
    }

    fn split_select_policies_for_source_sets(&self, select: &Select) -> Option<Vec<PolicyIr>> {
        if select_nullable_source_tables(select).is_empty() {
            return None;
        }
        let table_scope = TableScope::from_select(select);
        let mut policies = Vec::new();
        let mut changed = false;
        for policy in &self.policies {
            if policy.sources().len() <= 1 {
                policies.push(policy.clone());
                continue;
            }
            let Some(split) =
                split_policy_by_source_local_conjuncts(policy, &table_scope.direct_base_tables)
            else {
                policies.push(policy.clone());
                continue;
            };
            changed = true;
            policies.extend(split);
        }
        changed.then_some(policies)
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

    fn rewrite_expr_subqueries(&self, expr: &mut Expr) -> Result<(), RewriteError> {
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

    fn rewrite_derived_subqueries(&self, select: &mut Select) -> Result<(), RewriteError> {
        for table in &mut select.from {
            self.rewrite_derived_table_with_joins(table)?;
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

    fn rewrite_derived_table_factor(&self, factor: &mut TableFactor) -> Result<(), RewriteError> {
        if let TableFactor::Derived { subquery, .. } = factor {
            self.rewrite_query(subquery, None)?;
        }
        Ok(())
    }

    fn expand_insert_columns_for_invalidations(
        &self,
        insert: &mut sqlparser::ast::Insert,
        sink: &str,
    ) {
        let has_invalidate = self.policies.iter().any(|policy| {
            policy
                .sink()
                .is_some_and(|policy_sink| policy_sink.eq_ignore_ascii_case(sink))
                && matches!(
                    policy.resolution(),
                    Resolution::Invalidate | Resolution::InvalidateMessage
                )
        });
        if !has_invalidate || insert.columns.is_empty() {
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

    fn apply_aggregate_insert_columns(
        &self,
        insert: &mut sqlparser::ast::Insert,
        sink: &str,
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
            if !insert
                .columns
                .iter()
                .any(|column| column.value.eq_ignore_ascii_case(&temp_name))
            {
                insert.columns.push(Ident::new(&temp_name));
                let expr = aggregate_temp_projection_expr(&aggregate, is_query_aggregation)?;
                select.projection.push(SelectItem::ExprWithAlias {
                    expr,
                    alias: Ident::new(&temp_name),
                });
            }
        }
        Ok(())
    }

    fn source_aggregate_temp_columns(
        &self,
        sink: &str,
        table_scope: Option<&TableScope>,
    ) -> Result<Vec<(SourceAggregate, String)>, RewriteError> {
        let mut temp_columns = Vec::new();
        let mut seen = HashSet::new();
        for policy in &self.policies {
            let PolicyIr::CompatAggregate(policy) = policy else {
                continue;
            };
            if policy
                .sink
                .as_deref()
                .is_some_and(|policy_sink| !policy_sink.eq_ignore_ascii_case(sink))
            {
                continue;
            }
            if let Some(table_scope) = table_scope
                && !policy.sources.iter().all(|source| {
                    table_scope
                        .direct_base_tables
                        .contains(&source.to_ascii_lowercase())
                })
            {
                continue;
            }
            for aggregate in source_aggregates(&policy.constraint, &policy.sources)? {
                if seen.insert(aggregate.sql.clone()) {
                    let temp_name = aggregate_temp_column(temp_columns.len() + 1);
                    temp_columns.push((aggregate, temp_name));
                }
            }
        }
        Ok(temp_columns)
    }
}

fn policy_description(policy: &PolicyIr) -> Option<&str> {
    match policy {
        PolicyIr::CompatDfc { description, .. } => description.as_deref(),
        PolicyIr::CompatAggregate(policy) => policy.description.as_deref(),
        PolicyIr::NativeFlowGuard(policy) => policy.description.as_deref(),
    }
}

fn aggregate_finalize_sql(
    sink_table: &str,
    constraint: &str,
    dimensions: &[String],
) -> (String, String) {
    if dimensions.is_empty() {
        return (
            format!("SELECT ({constraint}) AS constraint_result FROM {sink_table}"),
            format!(
                "UPDATE {sink_table} SET valid = COALESCE(valid, true) AND (SELECT ({constraint}) FROM {sink_table})"
            ),
        );
    }

    let dimensions_sql = dimensions.join(", ");
    let inner_alias = "__passant_group";
    let inner_constraint = replace_table_prefix(constraint, sink_table, inner_alias);
    let join_predicate = dimensions
        .iter()
        .map(|dimension| {
            let inner_dimension = replace_table_prefix(dimension, sink_table, inner_alias);
            format!("{inner_dimension} = {dimension}")
        })
        .collect::<Vec<_>>()
        .join(" AND ");

    (
        format!(
            "SELECT {dimensions_sql}, ({constraint}) AS constraint_result FROM {sink_table} GROUP BY {dimensions_sql}"
        ),
        format!(
            "UPDATE {sink_table} SET valid = COALESCE(valid, true) AND COALESCE((SELECT ({inner_constraint}) FROM {sink_table} AS {inner_alias} WHERE {join_predicate}), true)"
        ),
    )
}

fn replace_table_prefix(sql: &str, table: &str, alias: &str) -> String {
    sql.replace(&format!("{table}."), &format!("{alias}."))
}

fn join_pushdown_policy_matches(policy: &PolicyIr, joined_base: &str) -> bool {
    matches!(
        policy,
        PolicyIr::CompatDfc {
            sources,
            required_sources,
            sink: None,
            on_fail: Resolution::Remove | Resolution::Kill | Resolution::Llm,
            ..
        } if required_sources.is_empty()
            && sources.len() == 1
            && sources[0].eq_ignore_ascii_case(joined_base)
    )
}

fn join_pushdown_expr(
    policy: &PolicyIr,
    base: &str,
    alias: Option<String>,
) -> Result<Expr, RewriteError> {
    let PolicyIr::CompatDfc {
        constraint,
        on_fail,
        ..
    } = policy
    else {
        return Err(RewriteError::Unsupported(
            "non-DFC policy cannot be pushed into joins".into(),
        ));
    };
    let mut expr = parse_expr(constraint)?;
    if let Some(alias) = alias {
        rewrite_column_qualifiers(
            &mut expr,
            &HashMap::from([(base.to_ascii_lowercase(), alias)]),
        );
    }
    expr = scan_policy_expr(
        expr,
        policy.sources(),
        &RewriteContext::default(),
        &HashMap::new(),
    )?;
    if *on_fail == Resolution::Kill {
        expr = kill_expr(expr)?;
    } else if *on_fail == Resolution::Llm {
        expr = resolver_expr(expr)?;
    }
    Ok(expr)
}

fn direct_source_occurrence_counts(select: &Select) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for table in &select.from {
        if let Some((base, _)) = table_factor_base_and_alias(&table.relation) {
            *counts.entry(base.to_ascii_lowercase()).or_default() += 1;
        }
        for join in &table.joins {
            if let Some((base, _)) = table_factor_base_and_alias(&join.relation) {
                *counts.entry(base.to_ascii_lowercase()).or_default() += 1;
            }
        }
    }
    counts
}

fn table_joins_all_inner(table: &TableWithJoins) -> bool {
    !table.joins.is_empty()
        && table
            .joins
            .iter()
            .all(|join| matches!(join.join_operator, JoinOperator::Inner(_)))
}

fn scan_policy_expr(
    mut expr: Expr,
    sources: &[String],
    context: &RewriteContext,
    alias_by_base: &HashMap<String, String>,
) -> Result<Expr, RewriteError> {
    let non_distributive = non_distributive_aggregates(&expr)?;
    if non_distributive.is_empty() {
        return Ok(strip_supported_aggregates(expr));
    }
    if context.sink.is_none() && !sources.is_empty() && expr_is_aggregate_only(&expr) {
        rewrite_column_qualifiers(&mut expr, &inverse_alias_map(alias_by_base));
        return scalar_policy_subquery_expr(expr, sources);
    }
    Err(RewriteError::Unsupported(format!(
        "non-distributive policy aggregate(s) require Partial-Push or LogicalFallback: {}",
        non_distributive.join(", ")
    )))
}

fn non_distributive_aggregates(expr: &Expr) -> Result<Vec<String>, RewriteError> {
    let aggregates = semiring::analyze_constraint(&expr.to_string())
        .map_err(|err| RewriteError::Unsupported(format!("policy aggregate analysis: {err}")))?;
    Ok(aggregates
        .into_iter()
        .filter(|aggregate| !aggregate.distributive)
        .map(|aggregate| aggregate.expression)
        .collect::<Vec<_>>())
}

fn scalar_policy_subquery_expr(expr: Expr, sources: &[String]) -> Result<Expr, RewriteError> {
    if let Expr::BinaryOp {
        left,
        op: BinaryOperator::And,
        right,
    } = expr
    {
        return Ok(and_expr(
            scalar_policy_subquery_expr(*left, sources)?,
            scalar_policy_subquery_expr(*right, sources)?,
        ));
    }

    let referenced_sources = referenced_source_tables(&expr, sources);
    let source = if referenced_sources.len() == 1 {
        referenced_sources[0].clone()
    } else if referenced_sources.is_empty() && sources.len() == 1 {
        sources[0].clone()
    } else {
        return Err(RewriteError::Unsupported(
            "non-distributive multi-source aggregate predicate requires Partial-Push or LogicalFallback".into(),
        ));
    };
    parse_expr(&format!("(SELECT {expr} FROM {source})"))
}

fn inverse_alias_map(alias_by_base: &HashMap<String, String>) -> HashMap<String, String> {
    alias_by_base
        .iter()
        .map(|(base, alias)| (alias.to_ascii_lowercase(), base.clone()))
        .collect()
}

fn referenced_source_tables(expr: &Expr, sources: &[String]) -> Vec<String> {
    let source_keys = sources
        .iter()
        .map(|source| source.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let mut referenced = HashSet::new();
    collect_referenced_source_tables(expr, &source_keys, &mut referenced);
    let mut referenced = referenced.into_iter().collect::<Vec<_>>();
    referenced.sort();
    referenced
}

fn collect_referenced_source_tables(
    expr: &Expr,
    source_keys: &HashSet<String>,
    referenced: &mut HashSet<String>,
) {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            let table = parts[0].value.to_ascii_lowercase();
            if source_keys.contains(&table) {
                referenced.insert(table);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_referenced_source_tables(left, source_keys, referenced);
            collect_referenced_source_tables(right, source_keys, referenced);
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => {
            collect_referenced_source_tables(expr, source_keys, referenced);
        }
        Expr::Function(function) => {
            collect_function_referenced_source_tables(function, source_keys, referenced);
            if let Some(filter) = function.filter.as_ref() {
                collect_referenced_source_tables(filter, source_keys, referenced);
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_referenced_source_tables(expr, source_keys, referenced);
            collect_referenced_source_tables(low, source_keys, referenced);
            collect_referenced_source_tables(high, source_keys, referenced);
        }
        Expr::InList { expr, list, .. } => {
            collect_referenced_source_tables(expr, source_keys, referenced);
            for item in list {
                collect_referenced_source_tables(item, source_keys, referenced);
            }
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                collect_referenced_source_tables(operand, source_keys, referenced);
            }
            for expr in conditions.iter().chain(results.iter()) {
                collect_referenced_source_tables(expr, source_keys, referenced);
            }
            if let Some(else_result) = else_result {
                collect_referenced_source_tables(else_result, source_keys, referenced);
            }
        }
        _ => {}
    }
}

fn collect_function_referenced_source_tables(
    function: &sqlparser::ast::Function,
    source_keys: &HashSet<String>,
    referenced: &mut HashSet<String>,
) {
    let FunctionArguments::List(args) = &function.args else {
        return;
    };
    for arg in &args.args {
        match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
            | FunctionArg::Named {
                arg: FunctionArgExpr::Expr(expr),
                ..
            }
            | FunctionArg::ExprNamed {
                arg: FunctionArgExpr::Expr(expr),
                ..
            } => collect_referenced_source_tables(expr, source_keys, referenced),
            _ => {}
        }
    }
}

fn expr_is_aggregate_only(expr: &Expr) -> bool {
    !expr_has_column_outside_aggregate(expr, false)
}

fn expr_has_column_outside_aggregate(expr: &Expr, inside_aggregate: bool) -> bool {
    match expr {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => !inside_aggregate,
        Expr::Function(function) => {
            let inside_aggregate =
                inside_aggregate || is_aggregate_name(&function.name.to_string());
            function_args_have_column_outside_aggregate(function, inside_aggregate)
                || function
                    .filter
                    .as_ref()
                    .is_some_and(|filter| expr_has_column_outside_aggregate(filter, false))
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_has_column_outside_aggregate(left, inside_aggregate)
                || expr_has_column_outside_aggregate(right, inside_aggregate)
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => expr_has_column_outside_aggregate(expr, inside_aggregate),
        Expr::Between {
            expr, low, high, ..
        } => {
            expr_has_column_outside_aggregate(expr, inside_aggregate)
                || expr_has_column_outside_aggregate(low, inside_aggregate)
                || expr_has_column_outside_aggregate(high, inside_aggregate)
        }
        Expr::InList { expr, list, .. } => {
            expr_has_column_outside_aggregate(expr, inside_aggregate)
                || list
                    .iter()
                    .any(|expr| expr_has_column_outside_aggregate(expr, inside_aggregate))
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            operand
                .as_deref()
                .is_some_and(|expr| expr_has_column_outside_aggregate(expr, inside_aggregate))
                || conditions
                    .iter()
                    .any(|expr| expr_has_column_outside_aggregate(expr, inside_aggregate))
                || results
                    .iter()
                    .any(|expr| expr_has_column_outside_aggregate(expr, inside_aggregate))
                || else_result
                    .as_deref()
                    .is_some_and(|expr| expr_has_column_outside_aggregate(expr, inside_aggregate))
        }
        _ => false,
    }
}

fn function_args_have_column_outside_aggregate(
    function: &sqlparser::ast::Function,
    inside_aggregate: bool,
) -> bool {
    let FunctionArguments::List(args) = &function.args else {
        return false;
    };
    args.args.iter().any(|arg| match arg {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
        | FunctionArg::Named {
            arg: FunctionArgExpr::Expr(expr),
            ..
        }
        | FunctionArg::ExprNamed {
            arg: FunctionArgExpr::Expr(expr),
            ..
        } => expr_has_column_outside_aggregate(expr, inside_aggregate),
        _ => false,
    })
}

fn table_factor_base_and_alias(factor: &TableFactor) -> Option<(String, Option<String>)> {
    match factor {
        TableFactor::Table { name, alias, .. } => Some((
            name.to_string(),
            alias.as_ref().map(|alias| alias.name.value.clone()),
        )),
        _ => None,
    }
}

fn policy_requires_set_split(
    policy: &PolicyIr,
    left_tables: &HashSet<String>,
    right_tables: &HashSet<String>,
) -> bool {
    let sources = policy
        .sources()
        .iter()
        .map(|source| source.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    sources.len() > 1
        && sources
            .iter()
            .all(|source| left_tables.contains(source) || right_tables.contains(source))
        && (!sources.iter().all(|source| left_tables.contains(source))
            || !sources.iter().all(|source| right_tables.contains(source)))
}

fn split_policy_for_set_branches(
    policy: &PolicyIr,
    left_tables: &HashSet<String>,
    right_tables: &HashSet<String>,
) -> Option<(Vec<PolicyIr>, Vec<PolicyIr>)> {
    let PolicyIr::CompatDfc {
        sources,
        required_sources,
        dimensions,
        sink,
        sink_alias,
        constraint,
        on_fail,
        description,
    } = policy
    else {
        return None;
    };
    if sink.is_some() || !required_sources.is_empty() || !dimensions.is_empty() {
        return None;
    }
    if !matches!(
        on_fail,
        Resolution::Remove | Resolution::Kill | Resolution::Llm
    ) {
        return None;
    }

    let policy_sources = sources
        .iter()
        .map(|source| source.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let expr = parse_expr(constraint).ok()?;
    let mut left_constraints = Vec::new();
    let mut right_constraints = Vec::new();
    for conjunct in split_conjuncts(expr) {
        let refs = expr_referenced_policy_sources(&conjunct, &policy_sources);
        if refs.is_empty() {
            left_constraints.push(conjunct.clone());
            right_constraints.push(conjunct);
            continue;
        }
        if refs.iter().all(|source| left_tables.contains(source)) {
            left_constraints.push(conjunct.clone());
        }
        if refs.iter().all(|source| right_tables.contains(source)) {
            right_constraints.push(conjunct);
        }
        if refs
            .iter()
            .any(|source| !left_tables.contains(source) && !right_tables.contains(source))
            || (!refs.iter().all(|source| left_tables.contains(source))
                && !refs.iter().all(|source| right_tables.contains(source)))
        {
            return None;
        }
    }

    let make_policy = |constraints: Vec<Expr>, tables: &HashSet<String>| {
        if constraints.is_empty() {
            return None;
        }
        let branch_sources = sources
            .iter()
            .filter(|source| tables.contains(&source.to_ascii_lowercase()))
            .cloned()
            .collect::<Vec<_>>();
        if branch_sources.is_empty() {
            return None;
        }
        Some(PolicyIr::CompatDfc {
            sources: branch_sources,
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: sink_alias.clone(),
            constraint: join_conjuncts(constraints).to_string(),
            on_fail: *on_fail,
            description: description.clone(),
        })
    };

    Some((
        make_policy(left_constraints, left_tables)
            .into_iter()
            .collect(),
        make_policy(right_constraints, right_tables)
            .into_iter()
            .collect(),
    ))
}

fn split_policy_by_source_local_conjuncts(
    policy: &PolicyIr,
    available_tables: &HashSet<String>,
) -> Option<Vec<PolicyIr>> {
    let PolicyIr::CompatDfc {
        sources,
        required_sources,
        dimensions,
        sink,
        sink_alias,
        constraint,
        on_fail,
        description,
    } = policy
    else {
        return None;
    };
    if sink.is_some() || !required_sources.is_empty() || !dimensions.is_empty() {
        return None;
    }
    if !matches!(
        on_fail,
        Resolution::Remove | Resolution::Kill | Resolution::Llm
    ) {
        return None;
    }

    let policy_sources = sources
        .iter()
        .map(|source| source.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let expr = parse_expr(constraint).ok()?;
    let mut constraints_by_source: HashMap<String, Vec<Expr>> = HashMap::new();
    for conjunct in split_conjuncts(expr) {
        let refs = expr_referenced_policy_sources(&conjunct, &policy_sources);
        if refs.len() != 1 {
            return None;
        }
        let source = refs.into_iter().next()?;
        if !available_tables.contains(&source) {
            return None;
        }
        constraints_by_source
            .entry(source)
            .or_default()
            .push(conjunct);
    }

    let mut split = Vec::new();
    for source in sources {
        let source_key = source.to_ascii_lowercase();
        let Some(constraints) = constraints_by_source.remove(&source_key) else {
            continue;
        };
        split.push(PolicyIr::CompatDfc {
            sources: vec![source.clone()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: sink_alias.clone(),
            constraint: join_conjuncts(constraints).to_string(),
            on_fail: *on_fail,
            description: description.clone(),
        });
    }
    (!split.is_empty()).then_some(split)
}

fn policy_applicability(
    policy: &PolicyIr,
    tables: &HashSet<String>,
    sink: Option<&str>,
) -> Option<PolicyApplicability> {
    let sink_matches = match policy.sink() {
        Some(policy_sink) => sink.is_some_and(|sink| sink.eq_ignore_ascii_case(policy_sink)),
        None => true,
    };
    if !sink_matches {
        return None;
    }

    if policy.sink().is_some() {
        let required_sources = policy
            .required_sources()
            .iter()
            .map(|source| source.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        let non_required_sources_match = policy.sources().iter().all(|source| {
            required_sources.contains(&source.to_ascii_lowercase())
                || tables.contains(&source.to_ascii_lowercase())
        });
        if !non_required_sources_match {
            return None;
        }
        if policy
            .required_sources()
            .iter()
            .any(|source| !tables.contains(&source.to_ascii_lowercase()))
        {
            return Some(PolicyApplicability::RequiredSourceMissing);
        }
        return Some(PolicyApplicability::Normal);
    }

    policy
        .sources()
        .iter()
        .all(|source| tables.contains(&source.to_ascii_lowercase()))
        .then_some(PolicyApplicability::Normal)
}

fn reject_policies_requiring_source_sets(
    applicable: &[(&PolicyIr, PolicyApplicability)],
    nullable_sources: &HashSet<String>,
) -> Result<(), RewriteError> {
    if nullable_sources.is_empty() {
        return Ok(());
    }

    let needs_source_sets = applicable.iter().any(|(policy, applicability)| {
        *applicability == PolicyApplicability::Normal
            && policy
                .sources()
                .iter()
                .any(|source| nullable_sources.contains(&source.to_ascii_lowercase()))
    });
    if needs_source_sets {
        return Err(RewriteError::Unsupported(
            "outer join policy enforcement for nullable sources requires source-set annotations"
                .into(),
        ));
    }

    Ok(())
}

fn prune_dominated_remove_policies(
    applicable: Vec<(&PolicyIr, PolicyApplicability)>,
) -> Vec<(&PolicyIr, PolicyApplicability)> {
    let mut keep = vec![true; applicable.len()];
    let mut strongest_by_key: HashMap<threshold::ThresholdKey, usize> = HashMap::new();

    for (index, (policy, applicability)) in applicable.iter().enumerate() {
        if *applicability != PolicyApplicability::Normal {
            continue;
        }
        let Some(candidate) = threshold::threshold_predicate_from_policy(policy) else {
            continue;
        };
        if let Some(existing_index) = strongest_by_key.get(&candidate.key).copied() {
            let Some(existing) =
                threshold::threshold_predicate_from_policy(applicable[existing_index].0)
            else {
                continue;
            };
            if threshold::threshold_dominates_predicates(&existing, &candidate) {
                keep[index] = false;
                continue;
            }
            if threshold::threshold_dominates_predicates(&candidate, &existing) {
                keep[existing_index] = false;
                strongest_by_key.insert(candidate.key.clone(), index);
            }
        } else {
            strongest_by_key.insert(candidate.key.clone(), index);
        }
    }

    applicable
        .into_iter()
        .enumerate()
        .filter(|(index, _)| keep[*index])
        .map(|(_, item)| item)
        .collect()
}

fn apply_resolution(
    select: &mut Select,
    expr: Expr,
    resolution: Resolution,
    description: Option<&str>,
    is_aggregation: bool,
) -> Result<(), RewriteError> {
    match resolution {
        Resolution::Remove => add_filter(select, expr, is_aggregation),
        Resolution::Kill => add_filter(select, kill_expr(expr)?, is_aggregation),
        Resolution::Invalidate => {
            upsert_select_projection(select, "valid", |existing| {
                Ok::<_, RewriteError>(
                    existing.map_or(expr.clone(), |existing| and_expr(existing, expr.clone())),
                )
            })?;
            Ok(())
        }
        Resolution::InvalidateMessage => {
            upsert_select_projection(select, "invalid_string", |existing| {
                if let Some(existing) = existing {
                    append_invalid_message_expr(existing, expr.clone(), description)
                } else {
                    invalidate_message_expr(expr.clone(), description)
                }
            })?;
            Ok(())
        }
        Resolution::Llm => add_filter(select, resolver_expr(expr)?, is_aggregation),
    }
}

fn upsert_select_projection<F, E>(select: &mut Select, name: &str, build_expr: F) -> Result<(), E>
where
    F: FnOnce(Option<Expr>) -> Result<Expr, E>,
{
    if let Some(position) = select.projection.iter().position(|item| {
        projection_expr_and_name(item)
            .and_then(|(_, alias)| alias)
            .is_some_and(|alias| alias.eq_ignore_ascii_case(name))
    }) {
        let existing =
            projection_expr_and_name(&select.projection[position]).map(|(expr, _)| expr.clone());
        select.projection[position] = SelectItem::ExprWithAlias {
            expr: build_expr(existing)?,
            alias: Ident::new(name),
        };
        return Ok(());
    }

    select.projection.push(SelectItem::ExprWithAlias {
        expr: build_expr(None)?,
        alias: Ident::new(name),
    });
    Ok(())
}

fn add_filter(select: &mut Select, expr: Expr, is_aggregation: bool) -> Result<(), RewriteError> {
    let target = if is_aggregation {
        &mut select.having
    } else {
        &mut select.selection
    };
    *target = Some(match target.take() {
        Some(existing) => and_expr(existing, expr),
        None => expr,
    });
    Ok(())
}

fn and_expr(left: Expr, right: Expr) -> Expr {
    Expr::BinaryOp {
        left: Box::new(left),
        op: BinaryOperator::And,
        right: Box::new(right),
    }
}

fn kill_expr(expr: Expr) -> Result<Expr, RewriteError> {
    parse_expr(&format!("({expr}) OR kill()"))
}

fn resolver_expr(expr: Expr) -> Result<Expr, RewriteError> {
    parse_expr(&format!(
        "CASE WHEN {expr} THEN true ELSE address_violating_rows() END"
    ))
}

fn invalidate_message_expr(expr: Expr, description: Option<&str>) -> Result<Expr, RewriteError> {
    let message = description
        .unwrap_or("DFC policy violation")
        .replace('\'', "''");
    parse_expr(&format!("CASE WHEN {expr} THEN NULL ELSE '{message}' END"))
}

fn append_invalid_message_expr(
    existing: Expr,
    expr: Expr,
    description: Option<&str>,
) -> Result<Expr, RewriteError> {
    let message = description
        .unwrap_or("DFC policy violation")
        .replace('\'', "''");
    parse_expr(&format!(
        "CASE WHEN {expr} THEN {existing} ELSE COALESCE({existing} || '; ', '') || '{message}' END"
    ))
}

fn parse_expr(sql: &str) -> Result<Expr, RewriteError> {
    let statement = parse_query(&format!("SELECT {sql}"))?;
    let Statement::Query(query) = statement else {
        return Err(RewriteError::Unsupported("constraint expression".into()));
    };
    let SetExpr::Select(select) = *query.body else {
        return Err(RewriteError::Unsupported("constraint expression".into()));
    };
    let Some(item) = select.projection.into_iter().next() else {
        return Err(RewriteError::Unsupported(
            "empty constraint expression".into(),
        ));
    };
    match item {
        SelectItem::UnnamedExpr(expr) => Ok(expr),
        SelectItem::ExprWithAlias { expr, .. } => Ok(expr),
        other => Err(RewriteError::Unsupported(format!(
            "constraint projection {other}"
        ))),
    }
}

fn set_expr_source_tables(set_expr: &SetExpr) -> HashSet<String> {
    match set_expr {
        SetExpr::Select(select) => select_source_tables(select),
        SetExpr::Query(query) => set_expr_source_tables(query.body.as_ref()),
        SetExpr::SetOperation { left, right, .. } => {
            let mut tables = set_expr_source_tables(left);
            tables.extend(set_expr_source_tables(right));
            tables
        }
        _ => HashSet::new(),
    }
}

fn select_source_tables(select: &Select) -> HashSet<String> {
    let mut tables = HashSet::new();
    for table in &select.from {
        tables.extend(table_with_joins_source_tables(table));
    }
    tables
}

fn table_with_joins_source_tables(table: &TableWithJoins) -> HashSet<String> {
    let mut tables = table_factor_source_tables(&table.relation);
    for join in &table.joins {
        tables.extend(table_factor_source_tables(&join.relation));
    }
    tables
}

fn select_nullable_source_tables(select: &Select) -> HashSet<String> {
    let mut nullable = HashSet::new();
    for table in &select.from {
        let mut left_tables = table_factor_source_tables(&table.relation);
        for join in &table.joins {
            let right_tables = table_factor_source_tables(&join.relation);
            match join.join_operator {
                JoinOperator::LeftOuter(_) => nullable.extend(right_tables.iter().cloned()),
                JoinOperator::RightOuter(_) => nullable.extend(left_tables.iter().cloned()),
                JoinOperator::FullOuter(_) => {
                    nullable.extend(left_tables.iter().cloned());
                    nullable.extend(right_tables.iter().cloned());
                }
                _ => {}
            }
            left_tables.extend(right_tables);
        }
    }
    nullable
}

fn select_has_full_join(select: &Select) -> bool {
    select.from.iter().any(|table| {
        table
            .joins
            .iter()
            .any(|join| matches!(join.join_operator, JoinOperator::FullOuter(_)))
    })
}

fn select_has_anti_join(select: &Select) -> bool {
    select.from.iter().any(|table| {
        table.joins.iter().any(|join| {
            matches!(
                join.join_operator,
                JoinOperator::Anti(_) | JoinOperator::LeftAnti(_) | JoinOperator::RightAnti(_)
            )
        })
    })
}

fn reject_policies_on_anti_probe_sources(
    select: &Select,
    applicable: &[(&PolicyIr, PolicyApplicability)],
) -> Result<(), RewriteError> {
    let probe_sources = select_anti_probe_source_tables(select);
    if probe_sources.is_empty() {
        return Ok(());
    }

    let has_probe_policy = applicable.iter().any(|(policy, applicability)| {
        *applicability == PolicyApplicability::Normal
            && policy
                .sources()
                .iter()
                .any(|source| probe_sources.contains(&source.to_ascii_lowercase()))
    });
    if has_probe_policy {
        return Err(RewriteError::Unsupported(
            "ANTI JOIN policy enforcement for probe-side sources requires source-set annotations"
                .into(),
        ));
    }

    Ok(())
}

fn select_anti_probe_source_tables(select: &Select) -> HashSet<String> {
    let mut probe_sources = HashSet::new();
    for table in &select.from {
        let mut left_tables = table_factor_source_tables(&table.relation);
        for join in &table.joins {
            let right_tables = table_factor_source_tables(&join.relation);
            match join.join_operator {
                JoinOperator::Anti(_) | JoinOperator::LeftAnti(_) => {
                    probe_sources.extend(right_tables.iter().cloned());
                }
                JoinOperator::RightAnti(_) => {
                    probe_sources.extend(left_tables.iter().cloned());
                }
                _ => {}
            }
            left_tables.extend(right_tables);
        }
    }
    probe_sources
}

fn table_factor_source_tables(factor: &TableFactor) -> HashSet<String> {
    match factor {
        TableFactor::Table { name, .. } => HashSet::from([name.to_string().to_ascii_lowercase()]),
        TableFactor::Derived { subquery, .. } => set_expr_source_tables(subquery.body.as_ref()),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => table_with_joins_source_tables(table_with_joins),
        _ => HashSet::new(),
    }
}

fn filter_table_factor(factor: &mut TableFactor, predicate: Expr) -> Result<(), RewriteError> {
    let TableFactor::Table { name, alias, .. } = factor else {
        return Ok(());
    };
    let alias_name = alias
        .as_ref()
        .map(|alias| alias.name.value.clone())
        .or_else(|| name.0.last().map(|part| part.value.clone()))
        .ok_or_else(|| RewriteError::Unsupported("table without name".into()))?;
    let statement = parse_query(&format!("SELECT * FROM {name} WHERE {predicate}"))?;
    let Statement::Query(subquery) = statement else {
        return Err(RewriteError::Unsupported("full join source filter".into()));
    };
    *factor = TableFactor::Derived {
        lateral: false,
        subquery,
        alias: Some(TableAlias {
            name: Ident::new(alias_name),
            columns: Vec::new(),
        }),
    };
    Ok(())
}

fn select_is_aggregation(select: &Select) -> bool {
    !select.group_by.is_empty()
        || select.having.is_some()
        || select.projection.iter().any(select_item_contains_aggregate)
}

fn select_item_contains_aggregate(item: &SelectItem) -> bool {
    match item {
        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
            expr_contains_aggregate(expr)
        }
        _ => false,
    }
}

fn outer_limited_projection(select: &Select) -> String {
    select
        .projection
        .iter()
        .map(|item| match item {
            SelectItem::UnnamedExpr(expr) => {
                projected_column_name(expr).unwrap_or_else(|| expr.to_string())
            }
            SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn projected_select_names(select: &Select) -> HashSet<String> {
    select
        .projection
        .iter()
        .filter_map(|item| match item {
            SelectItem::UnnamedExpr(expr) => projected_column_name(expr),
            SelectItem::ExprWithAlias { alias, .. } => Some(alias.value.clone()),
            _ => None,
        })
        .map(|name| name.to_ascii_lowercase())
        .collect()
}

fn expr_contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function(function) => is_aggregate_name(&function.name.to_string()),
        Expr::BinaryOp { left, right, .. } => {
            expr_contains_aggregate(left) || expr_contains_aggregate(right)
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => expr_contains_aggregate(expr),
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            operand.as_deref().is_some_and(expr_contains_aggregate)
                || conditions.iter().any(expr_contains_aggregate)
                || results.iter().any(expr_contains_aggregate)
                || else_result.as_deref().is_some_and(expr_contains_aggregate)
        }
        _ => false,
    }
}

fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count" | "sum" | "avg" | "min" | "max" | "array_agg" | "bool_and" | "bool_or"
    )
}

fn strip_supported_aggregates(expr: Expr) -> Expr {
    match expr {
        Expr::Function(function) if is_aggregate_name(&function.name.to_string()) => {
            first_function_expr(&function).unwrap_or(Expr::Function(function))
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(strip_supported_aggregates(*left)),
            op,
            right: Box::new(strip_supported_aggregates(*right)),
        },
        Expr::Nested(expr) => Expr::Nested(Box::new(strip_supported_aggregates(*expr))),
        Expr::UnaryOp { op, expr } => Expr::UnaryOp {
            op,
            expr: Box::new(strip_supported_aggregates(*expr)),
        },
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => Expr::Case {
            operand: operand.map(|expr| Box::new(strip_supported_aggregates(*expr))),
            conditions: conditions
                .into_iter()
                .map(strip_supported_aggregates)
                .collect(),
            results: results
                .into_iter()
                .map(strip_supported_aggregates)
                .collect(),
            else_result: else_result.map(|expr| Box::new(strip_supported_aggregates(*expr))),
        },
        other => other,
    }
}

fn first_function_expr(function: &sqlparser::ast::Function) -> Option<Expr> {
    let FunctionArguments::List(args) = &function.args else {
        return None;
    };
    let first = args.args.first()?;
    match first {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
        | FunctionArg::Named {
            arg: FunctionArgExpr::Expr(expr),
            ..
        }
        | FunctionArg::ExprNamed {
            arg: FunctionArgExpr::Expr(expr),
            ..
        } => Some(expr.clone()),
        _ => None,
    }
}

fn rewrite_source_aggregates_for_finalize(
    constraint: &str,
    sources: &[String],
    aggregate_temp_columns: &[(SourceAggregate, String)],
) -> Result<String, RewriteError> {
    let mut rewritten = constraint.to_string();
    for aggregate in source_aggregates(constraint, sources)? {
        if let Some((_, temp_name)) = aggregate_temp_columns
            .iter()
            .find(|(candidate, _)| candidate.sql == aggregate.sql)
        {
            let replacement = format!(
                "{}({})",
                aggregate_finalize_function_name(&aggregate.function_name),
                temp_name
            );
            rewritten = rewritten.replace(&aggregate.sql, &replacement);
        }
    }
    Ok(rewritten)
}

fn source_aggregates(
    constraint: &str,
    sources: &[String],
) -> Result<Vec<SourceAggregate>, RewriteError> {
    let expr = parse_expr(constraint)?;
    let mut aggregates = Vec::new();
    collect_source_aggregates(&expr, sources, &mut aggregates);
    Ok(aggregates)
}

fn collect_source_aggregates(
    expr: &Expr,
    sources: &[String],
    aggregates: &mut Vec<SourceAggregate>,
) {
    match expr {
        Expr::Function(function) if is_aggregate_name(&function.name.to_string()) => {
            if let Some(input) = first_function_expr(function)
                && expr_references_any_source(&input, sources)
            {
                aggregates.push(SourceAggregate {
                    sql: expr.to_string(),
                    function_name: function.name.to_string(),
                    expr: expr.clone(),
                    input,
                });
            }
        }
        Expr::Function(function) => {
            if let FunctionArguments::List(args) = &function.args {
                for arg in &args.args {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                        | FunctionArg::Named {
                            arg: FunctionArgExpr::Expr(expr),
                            ..
                        }
                        | FunctionArg::ExprNamed {
                            arg: FunctionArgExpr::Expr(expr),
                            ..
                        } => collect_source_aggregates(expr, sources, aggregates),
                        _ => {}
                    }
                }
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_source_aggregates(left, sources, aggregates);
            collect_source_aggregates(right, sources, aggregates);
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => collect_source_aggregates(expr, sources, aggregates),
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                collect_source_aggregates(operand, sources, aggregates);
            }
            for expr in conditions.iter().chain(results.iter()) {
                collect_source_aggregates(expr, sources, aggregates);
            }
            if let Some(else_result) = else_result {
                collect_source_aggregates(else_result, sources, aggregates);
            }
        }
        _ => {}
    }
}

fn aggregate_temp_projection_expr(
    aggregate: &SourceAggregate,
    is_query_aggregation: bool,
) -> Result<Expr, RewriteError> {
    if is_query_aggregation {
        return Ok(aggregate.expr.clone());
    }
    if aggregate.function_name.eq_ignore_ascii_case("count") {
        return parse_expr(&format!(
            "CASE WHEN {} IS NOT NULL THEN 1 ELSE 0 END",
            aggregate.input
        ));
    }
    Ok(aggregate.input.clone())
}

fn aggregate_finalize_function_name(function_name: &str) -> &str {
    if function_name.eq_ignore_ascii_case("count") {
        "sum"
    } else {
        function_name
    }
}

fn expr_references_any_source(expr: &Expr, sources: &[String]) -> bool {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => sources
            .iter()
            .any(|source| parts[0].value.eq_ignore_ascii_case(source)),
        Expr::BinaryOp { left, right, .. } => {
            expr_references_any_source(left, sources) || expr_references_any_source(right, sources)
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => expr_references_any_source(expr, sources),
        Expr::Function(function) => {
            if let FunctionArguments::List(args) = &function.args {
                return args.args.iter().any(|arg| match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                    | FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(expr),
                        ..
                    }
                    | FunctionArg::ExprNamed {
                        arg: FunctionArgExpr::Expr(expr),
                        ..
                    } => expr_references_any_source(expr, sources),
                    _ => false,
                });
            }
            false
        }
        _ => false,
    }
}

fn expr_referenced_policy_sources(
    expr: &Expr,
    policy_sources: &HashSet<String>,
) -> HashSet<String> {
    let mut refs = HashSet::new();
    collect_referenced_policy_sources(expr, policy_sources, &mut refs);
    refs
}

fn collect_referenced_policy_sources(
    expr: &Expr,
    policy_sources: &HashSet<String>,
    refs: &mut HashSet<String>,
) {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            let table = parts[0].value.to_ascii_lowercase();
            if policy_sources.contains(&table) {
                refs.insert(table);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_referenced_policy_sources(left, policy_sources, refs);
            collect_referenced_policy_sources(right, policy_sources, refs);
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => {
            collect_referenced_policy_sources(expr, policy_sources, refs);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_referenced_policy_sources(expr, policy_sources, refs);
            collect_referenced_policy_sources(low, policy_sources, refs);
            collect_referenced_policy_sources(high, policy_sources, refs);
        }
        Expr::InList { expr, list, .. } => {
            collect_referenced_policy_sources(expr, policy_sources, refs);
            for expr in list {
                collect_referenced_policy_sources(expr, policy_sources, refs);
            }
        }
        Expr::Function(function) => {
            if let FunctionArguments::List(args) = &function.args {
                for arg in &args.args {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                        | FunctionArg::Named {
                            arg: FunctionArgExpr::Expr(expr),
                            ..
                        }
                        | FunctionArg::ExprNamed {
                            arg: FunctionArgExpr::Expr(expr),
                            ..
                        } => collect_referenced_policy_sources(expr, policy_sources, refs),
                        _ => {}
                    }
                }
            }
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                collect_referenced_policy_sources(operand, policy_sources, refs);
            }
            for expr in conditions.iter().chain(results.iter()) {
                collect_referenced_policy_sources(expr, policy_sources, refs);
            }
            if let Some(else_result) = else_result {
                collect_referenced_policy_sources(else_result, policy_sources, refs);
            }
        }
        _ => {}
    }
}

fn split_conjuncts(expr: Expr) -> Vec<Expr> {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            let mut conjuncts = split_conjuncts(*left);
            conjuncts.extend(split_conjuncts(*right));
            conjuncts
        }
        Expr::Nested(expr) => split_conjuncts(*expr),
        expr => vec![expr],
    }
}

fn join_conjuncts(mut conjuncts: Vec<Expr>) -> Expr {
    let first = conjuncts.remove(0);
    conjuncts.into_iter().fold(first, and_expr)
}

fn aggregate_temp_column(index: usize) -> String {
    format!("_passant_agg_{index}")
}

fn insert_select_mapping(insert: &sqlparser::ast::Insert) -> HashMap<String, Expr> {
    let mut mapping = HashMap::new();
    let Some(query) = insert.source.as_ref() else {
        return mapping;
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return mapping;
    };

    for (index, item) in select.projection.iter().enumerate() {
        let Some((expr, alias)) = projection_expr_and_name(item) else {
            continue;
        };
        if let Some(column) = insert.columns.get(index) {
            mapping.insert(column.value.to_ascii_lowercase(), expr.clone());
        }
        if let Some(alias) = alias {
            mapping
                .entry(alias.to_ascii_lowercase())
                .or_insert(expr.clone());
        }
    }
    mapping
}

fn update_target_name(table: &TableWithJoins) -> Option<String> {
    match &table.relation {
        TableFactor::Table { name, .. } => Some(name.to_string()),
        _ => None,
    }
}

fn update_assignment_mapping(assignments: &[Assignment]) -> HashMap<String, Expr> {
    let mut mapping = HashMap::new();
    for assignment in assignments {
        let AssignmentTarget::ColumnName(name) = &assignment.target else {
            continue;
        };
        if let Some(column) = name.0.last() {
            mapping.insert(column.value.to_ascii_lowercase(), assignment.value.clone());
        }
    }
    mapping
}

fn upsert_valid_assignment(assignments: &mut Vec<Assignment>, value: Expr) {
    for assignment in assignments.iter_mut() {
        let AssignmentTarget::ColumnName(name) = &assignment.target else {
            continue;
        };
        if name
            .0
            .last()
            .is_some_and(|ident| ident.value.eq_ignore_ascii_case("valid"))
        {
            assignment.value = and_expr(assignment.value.clone(), value);
            return;
        }
    }

    assignments.push(Assignment {
        target: AssignmentTarget::ColumnName(ObjectName(vec![Ident::new("valid")])),
        value,
    });
}

fn upsert_invalid_string_assignment(
    assignments: &mut Vec<Assignment>,
    value: Expr,
    description: Option<&str>,
) -> Result<(), RewriteError> {
    for assignment in assignments.iter_mut() {
        let AssignmentTarget::ColumnName(name) = &assignment.target else {
            continue;
        };
        if name
            .0
            .last()
            .is_some_and(|ident| ident.value.eq_ignore_ascii_case("invalid_string"))
        {
            assignment.value =
                append_invalid_message_expr(assignment.value.clone(), value, description)?;
            return Ok(());
        }
    }

    assignments.push(Assignment {
        target: AssignmentTarget::ColumnName(ObjectName(vec![Ident::new("invalid_string")])),
        value: invalidate_message_expr(value, description)?,
    });
    Ok(())
}

fn projection_expr_and_name(item: &SelectItem) -> Option<(&Expr, Option<String>)> {
    match item {
        SelectItem::UnnamedExpr(expr) => Some((expr, projected_column_name(expr))),
        SelectItem::ExprWithAlias { expr, alias } => Some((expr, Some(alias.value.clone()))),
        _ => None,
    }
}

fn projected_column_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(ident) => Some(ident.value.clone()),
        Expr::CompoundIdentifier(parts) => parts.last().map(|ident| ident.value.clone()),
        _ => None,
    }
}

fn replace_sink_columns(
    expr: Expr,
    sink: &str,
    sink_expr_by_column: &HashMap<String, Expr>,
) -> Expr {
    match expr {
        Expr::CompoundIdentifier(parts)
            if parts.len() >= 2 && parts[0].value.eq_ignore_ascii_case(sink) =>
        {
            sink_expr_by_column
                .get(&parts[1].value.to_ascii_lowercase())
                .cloned()
                .unwrap_or(Expr::CompoundIdentifier(parts))
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(replace_sink_columns(*left, sink, sink_expr_by_column)),
            op,
            right: Box::new(replace_sink_columns(*right, sink, sink_expr_by_column)),
        },
        Expr::Nested(expr) => Expr::Nested(Box::new(replace_sink_columns(
            *expr,
            sink,
            sink_expr_by_column,
        ))),
        Expr::UnaryOp { op, expr } => Expr::UnaryOp {
            op,
            expr: Box::new(replace_sink_columns(*expr, sink, sink_expr_by_column)),
        },
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => Expr::Case {
            operand: operand
                .map(|expr| Box::new(replace_sink_columns(*expr, sink, sink_expr_by_column))),
            conditions: conditions
                .into_iter()
                .map(|expr| replace_sink_columns(expr, sink, sink_expr_by_column))
                .collect(),
            results: results
                .into_iter()
                .map(|expr| replace_sink_columns(expr, sink, sink_expr_by_column))
                .collect(),
            else_result: else_result
                .map(|expr| Box::new(replace_sink_columns(*expr, sink, sink_expr_by_column))),
        },
        Expr::Function(mut function) => {
            replace_sink_columns_in_function(&mut function, sink, sink_expr_by_column);
            Expr::Function(function)
        }
        other => other,
    }
}

fn collect_compound_columns_by_name(expr: &Expr, columns: &mut HashMap<String, Expr>) {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            if let Some(column) = parts.last() {
                columns
                    .entry(column.value.to_ascii_lowercase())
                    .or_insert_with(|| expr.clone());
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_compound_columns_by_name(left, columns);
            collect_compound_columns_by_name(right, columns);
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => collect_compound_columns_by_name(expr, columns),
        Expr::Function(function) => {
            if let FunctionArguments::List(args) = &function.args {
                for arg in &args.args {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                        | FunctionArg::Named {
                            arg: FunctionArgExpr::Expr(expr),
                            ..
                        }
                        | FunctionArg::ExprNamed {
                            arg: FunctionArgExpr::Expr(expr),
                            ..
                        } => collect_compound_columns_by_name(expr, columns),
                        _ => {}
                    }
                }
            }
            if let Some(filter) = &function.filter {
                collect_compound_columns_by_name(filter, columns);
            }
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                collect_compound_columns_by_name(operand, columns);
            }
            for expr in conditions.iter().chain(results.iter()) {
                collect_compound_columns_by_name(expr, columns);
            }
            if let Some(else_result) = else_result {
                collect_compound_columns_by_name(else_result, columns);
            }
        }
        _ => {}
    }
}

fn replace_identifiers(expr: &mut Expr, replacements: &HashMap<String, String>) {
    match expr {
        Expr::Identifier(ident) => {
            if let Some(replacement) = replacements.get(&ident.value.to_ascii_lowercase()) {
                *ident = Ident::new(replacement);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            replace_identifiers(left, replacements);
            replace_identifiers(right, replacements);
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => replace_identifiers(expr, replacements),
        Expr::Function(function) => replace_identifiers_in_function(function, replacements),
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                replace_identifiers(operand, replacements);
            }
            for expr in conditions.iter_mut().chain(results.iter_mut()) {
                replace_identifiers(expr, replacements);
            }
            if let Some(else_result) = else_result {
                replace_identifiers(else_result, replacements);
            }
        }
        _ => {}
    }
}

fn replace_identifiers_in_function(
    function: &mut sqlparser::ast::Function,
    replacements: &HashMap<String, String>,
) {
    let FunctionArguments::List(args) = &mut function.args else {
        return;
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
            } => replace_identifiers(expr, replacements),
            _ => {}
        }
    }
    if let Some(filter) = &mut function.filter {
        replace_identifiers(filter, replacements);
    }
}

fn replace_sink_columns_in_function(
    function: &mut sqlparser::ast::Function,
    sink: &str,
    sink_expr_by_column: &HashMap<String, Expr>,
) {
    let FunctionArguments::List(args) = &mut function.args else {
        return;
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
            } => {
                *expr = replace_sink_columns(expr.clone(), sink, sink_expr_by_column);
            }
            _ => {}
        }
    }
}

#[derive(Debug, Default)]
struct TableScope {
    base_tables: HashSet<String>,
    direct_base_tables: HashSet<String>,
    alias_by_base: HashMap<String, String>,
}

impl TableScope {
    fn from_select(select: &Select) -> Self {
        let mut scope = Self::default();
        for table in &select.from {
            scope.add_table_with_joins(table);
        }
        scope
    }

    fn add_table_with_joins(&mut self, table: &TableWithJoins) {
        self.add_table_factor(&table.relation, true);
        for join in &table.joins {
            self.add_table_factor(&join.relation, true);
        }
    }

    fn add_table_factor(&mut self, factor: &TableFactor, is_direct: bool) {
        match factor {
            TableFactor::Table { name, alias, .. } => {
                let base = name.to_string();
                let key = base.to_ascii_lowercase();
                self.base_tables.insert(key.clone());
                if is_direct {
                    self.direct_base_tables.insert(key.clone());
                }
                if let Some(alias) = alias {
                    self.alias_by_base.insert(key, alias.name.value.clone());
                }
            }
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                if let Some(alias) = alias {
                    self.base_tables
                        .insert(alias.name.value.to_ascii_lowercase());
                }
                if let SetExpr::Select(select) = subquery.body.as_ref() {
                    for table in &select.from {
                        self.add_table_factor(&table.relation, false);
                        for join in &table.joins {
                            self.add_table_factor(&join.relation, false);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn rewrite_column_qualifiers(expr: &mut Expr, alias_by_base: &HashMap<String, String>) {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            if let Some(alias) = alias_by_base.get(&parts[0].value.to_ascii_lowercase()) {
                parts[0] = Ident::new(alias);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            rewrite_column_qualifiers(left, alias_by_base);
            rewrite_column_qualifiers(right, alias_by_base);
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => rewrite_column_qualifiers(expr, alias_by_base),
        Expr::Function(function) => {
            rewrite_function_args(function, alias_by_base);
            if let Some(filter) = function.filter.as_mut() {
                rewrite_column_qualifiers(filter, alias_by_base);
            }
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                rewrite_column_qualifiers(operand, alias_by_base);
            }
            for expr in conditions.iter_mut().chain(results.iter_mut()) {
                rewrite_column_qualifiers(expr, alias_by_base);
            }
            if let Some(else_result) = else_result {
                rewrite_column_qualifiers(else_result, alias_by_base);
            }
        }
        _ => {}
    }
}

fn unqualify_columns(expr: &mut Expr) {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            if let Some(column) = parts.last().cloned() {
                *expr = Expr::Identifier(column);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            unqualify_columns(left);
            unqualify_columns(right);
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => unqualify_columns(expr),
        Expr::Function(function) => {
            unqualify_function_args(function);
            if let Some(filter) = function.filter.as_mut() {
                unqualify_columns(filter);
            }
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                unqualify_columns(operand);
            }
            for expr in conditions.iter_mut().chain(results.iter_mut()) {
                unqualify_columns(expr);
            }
            if let Some(else_result) = else_result {
                unqualify_columns(else_result);
            }
        }
        _ => {}
    }
}

fn unqualify_function_args(function: &mut sqlparser::ast::Function) {
    let FunctionArguments::List(args) = &mut function.args else {
        return;
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
            } => unqualify_columns(expr),
            _ => {}
        }
    }
}

fn rewrite_function_args(
    function: &mut sqlparser::ast::Function,
    alias_by_base: &HashMap<String, String>,
) {
    let FunctionArguments::List(args) = &mut function.args else {
        return;
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
            } => rewrite_column_qualifiers(expr, alias_by_base),
            _ => {}
        }
    }
}

trait GroupByEmpty {
    fn is_empty(&self) -> bool;
}

impl GroupByEmpty for sqlparser::ast::GroupByExpr {
    fn is_empty(&self) -> bool {
        match self {
            sqlparser::ast::GroupByExpr::All(_) => false,
            sqlparser::ast::GroupByExpr::Expressions(exprs, _) => exprs.is_empty(),
        }
    }
}

#[allow(dead_code)]
fn _object_name(name: &str) -> ObjectName {
    ObjectName(vec![Ident::new(name)])
}

fn bool_literal(value: bool) -> Expr {
    Expr::Value(Value::Boolean(value))
}
