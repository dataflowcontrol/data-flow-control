use std::collections::{HashMap, HashSet};

use sqlparser::ast::{
    Assignment, AssignmentTarget, BinaryOperator, DuplicateTreatment, Expr, FunctionArg,
    FunctionArgExpr, FunctionArguments, GroupByExpr, Ident, JoinConstraint, JoinOperator,
    MergeAction, ObjectName, Query, Select, SelectItem, SetExpr, SetOperator, Statement,
    TableAlias, TableFactor, TableWithJoins, Value,
};
use thiserror::Error;

use crate::parser::{ParseError, parse_query};
use crate::policy::{PolicyIr, PolicyParseError, Resolution, parse_policy_text};
use crate::semiring;
use crate::source_sets::{
    cross_source_policies_for_branch, select_has_anti_join, select_has_full_join,
    select_nullable_source_tables, set_expr_source_tables,
    set_operation_requires_cross_source_policies, split_select_policies_for_nullable_joins,
    split_set_operation_policies, table_factor_source_tables,
};
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
pub struct TableCatalog {
    table_columns: HashMap<String, Vec<String>>,
    unique_columns: HashSet<(String, String)>,
}

impl TableCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_table(&mut self, table: impl Into<String>, columns: Vec<String>) {
        self.table_columns
            .insert(table.into().to_ascii_lowercase(), columns);
    }

    pub fn register_unique_column(&mut self, table: impl Into<String>, column: impl Into<String>) {
        self.unique_columns.insert((
            table.into().to_ascii_lowercase(),
            column.into().to_ascii_lowercase(),
        ));
    }

    pub fn columns(&self, table: &str) -> Option<&[String]> {
        self.table_columns
            .get(&table.to_ascii_lowercase())
            .map(Vec::as_slice)
    }

    pub fn is_unique_column(&self, table: &str, column: &str) -> bool {
        self.unique_columns
            .contains(&(table.to_ascii_lowercase(), column.to_ascii_lowercase()))
    }
}

#[derive(Debug, Default, Clone)]
pub struct PassantRewriter {
    policies: Vec<PolicyIr>,
    catalog: TableCatalog,
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
    is_sink_aggregate: bool,
}

#[derive(Debug, Clone, Default)]
struct RewriteContext {
    sink: Option<String>,
    sink_expr_by_column: HashMap<String, Expr>,
    allow_partial_source_visibility: bool,
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

    pub fn with_catalog(catalog: TableCatalog) -> Self {
        Self {
            policies: Vec::new(),
            catalog,
        }
    }

    pub fn catalog(&self) -> &TableCatalog {
        &self.catalog
    }

    pub fn catalog_mut(&mut self) -> &mut TableCatalog {
        &mut self.catalog
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
        if let Some(rewritten) = self.rewrite_limited_query(&statement)? {
            return Ok(restore_inner_join_keyword(sql, rewritten));
        }
        self.rewrite_statement(&mut statement)?;
        let rewritten = statement.to_string();
        Ok(restore_inner_join_keyword(
            sql,
            normalize_distinct_casing(restore_preserved_aggregate_sql(
                rewritten,
                &self.policies,
            )),
        ))
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
            policy_applicability(policy, &table_scope.direct_base_tables, None, false)
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
                    allow_partial_source_visibility: false,
                };
                let Some(source) = insert.source.as_mut() else {
                    return Ok(());
                };
                let before_columns = insert.columns.len();
                self.rewrite_query_with_context(source, &context)?;
                self.apply_aggregate_insert_columns(insert, &sink, &context)?;
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
            Statement::Merge {
                table,
                source,
                on,
                clauses,
                ..
            } => self.rewrite_merge(table, source, on, clauses),
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
                            expr = replace_sink_columns(expr, "_OUTPUT_", &context.sink_expr_by_column);
                            if let Some(sink_alias) = sink_alias {
                                expr = replace_sink_columns(expr, sink_alias, &context.sink_expr_by_column);
                            }
                        }
                        rewrite_column_qualifiers(&mut expr, &table_scope.alias_by_base);
                        scan_policy_expr(expr, policy.sources(), &context, &table_scope.alias_by_base)?
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
                .all(|source| source_tables.contains(&source.to_ascii_lowercase()))
            {
                continue;
            }
            let context = RewriteContext::default();
            let mut expr = parse_expr(constraint)?;
            expr = scan_policy_expr(expr, policy.sources(), &context, &HashMap::new())?;
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

    fn rewrite_query(&self, query: &mut Query, sink: Option<&str>) -> Result<(), RewriteError> {
        let context = RewriteContext {
            sink: sink.map(str::to_string),
            sink_expr_by_column: HashMap::new(),
            allow_partial_source_visibility: false,
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
            SetExpr::SetOperation { op, left, right, .. } => {
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

    fn rewrite_select(
        &self,
        select: &mut Select,
        context: &RewriteContext,
    ) -> Result<(), RewriteError> {
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
            .filter(|(index, _)| !pushed_policy_indices.contains(index))
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
                    .all(|source| bases.contains(&source.to_ascii_lowercase()))
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
                        relation_filter.push(join_pushdown_expr(policy, &base, None, &self.catalog)?);
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
                            join_filter.push(join_pushdown_expr(policy, &base, None, &self.catalog)?);
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
        if !has_invalidate {
            return;
        }

        if insert.columns.is_empty() {
            if let Some(catalog_columns) = self.catalog.columns(sink) {
                insert.columns = catalog_columns
                    .iter()
                    .filter(|column| !column.eq_ignore_ascii_case("valid"))
                    .filter(|column| !column.eq_ignore_ascii_case("invalid_string"))
                    .map(Ident::new)
                    .collect();
            }
            if insert.columns.is_empty() {
                return;
            }
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

    fn apply_aggregate_scan_columns(&self, select: &mut Select) -> Result<(), RewriteError> {
        let table_scope = TableScope::from_select(select);
        for (aggregate, temp_name) in self.scan_aggregate_temp_columns(&table_scope)? {
            select.projection.push(SelectItem::ExprWithAlias {
                expr: parse_expr(&aggregate.sql)?,
                alias: Ident::new(&temp_name),
            });
        }
        Ok(())
    }

    fn scan_aggregate_temp_columns(
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
                    .contains(&source.to_ascii_lowercase())
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
            if let Some(table_scope) = table_scope {
                let sources_visible = policy.sources.is_empty()
                    || policy.sources.iter().all(|source| {
                        table_scope
                            .direct_base_tables
                            .contains(&source.to_ascii_lowercase())
                    });
                if !sources_visible {
                    continue;
                }
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
}

fn policy_description(policy: &PolicyIr) -> Option<&str> {
    match policy {
        PolicyIr::CompatDfc { description, .. } => description.as_deref(),
        PolicyIr::CompatAggregate(policy) => policy.description.as_deref(),
        PolicyIr::NativePgn(policy) => policy.description.as_deref(),
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
    catalog: &TableCatalog,
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
    if let Some(guard) = unique_column_guard_from_constraint(constraint, catalog) {
        expr = and_expr(guard, expr);
    }
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

fn table_with_joins_base_tables(table: &TableWithJoins) -> HashSet<String> {
    let mut bases = HashSet::new();
    if let Some((base, _)) = table_factor_base_and_alias(&table.relation) {
        bases.insert(base.to_ascii_lowercase());
    }
    for join in &table.joins {
        if let Some((base, _)) = table_factor_base_and_alias(&join.relation) {
            bases.insert(base.to_ascii_lowercase());
        }
    }
    bases
}

fn scan_policy_expr(
    mut expr: Expr,
    sources: &[String],
    context: &RewriteContext,
    alias_by_base: &HashMap<String, String>,
) -> Result<Expr, RewriteError> {
    let non_distributive = non_distributive_aggregates(&expr)?;
    if non_distributive.is_empty() {
        return transform_scan_aggregates(expr);
    }
    if context.sink.is_none() && !sources.is_empty() && expr_is_aggregate_only(&expr) {
        if non_distributive
            .iter()
            .all(|aggregate| is_scan_transformable_non_distributive(aggregate))
        {
            let transformed = transform_scan_aggregates(expr.clone())?;
            if !expr_contains_aggregate(&transformed) {
                let mut transformed = transformed;
                rewrite_column_qualifiers(&mut transformed, &inverse_alias_map(alias_by_base));
                return Ok(transformed);
            }
        }
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

fn policy_applicability(
    policy: &PolicyIr,
    tables: &HashSet<String>,
    sink: Option<&str>,
    allow_partial_source_visibility: bool,
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
        .or_else(|| {
            if allow_partial_source_visibility
                && policy.sink().is_none()
                && policy.required_sources().is_empty()
                && policy.sources().len() > 1
                && policy.sources().iter().any(|source| {
                    tables.contains(&source.to_ascii_lowercase())
                })
            {
                Some(PolicyApplicability::Normal)
            } else {
                None
            }
        })
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

fn build_compat_dfc_filter_expr(
    sources: &[String],
    constraint: &str,
    sink_alias: &Option<String>,
    applicability: PolicyApplicability,
    context: &RewriteContext,
    table_scope: &TableScope,
    is_aggregation: bool,
) -> Result<Expr, RewriteError> {
    if applicability == PolicyApplicability::RequiredSourceMissing {
        return Ok(bool_literal(false));
    }
    if sources.len() == 1 {
        let base = sources[0].to_ascii_lowercase();
        if let Some(aliases) = table_scope.aliases_by_base.get(&base)
            && aliases.len() > 1
        {
                let filters = aliases
                    .iter()
                    .map(|alias| {
                        build_single_alias_compat_dfc_filter_expr(
                            constraint,
                            sink_alias,
                            context,
                            sources,
                            &base,
                            alias,
                            is_aggregation,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok(join_conjuncts(filters));
        }
        return build_single_alias_compat_dfc_filter_expr(
            constraint,
            sink_alias,
            context,
            sources,
            &base,
            table_scope
                .alias_by_base
                .get(&base)
                .map(String::as_str)
                .unwrap_or(sources[0].as_str()),
            is_aggregation,
        );
    }
    let mut expr = parse_expr(constraint)?;
    if let Some(sink) = &context.sink {
        expr = replace_sink_columns(expr, sink, &context.sink_expr_by_column);
        expr = replace_sink_columns(expr, "_OUTPUT_", &context.sink_expr_by_column);
        if let Some(sink_alias) = sink_alias {
            expr = replace_sink_columns(expr, sink_alias, &context.sink_expr_by_column);
        }
    }
    rewrite_column_qualifiers(&mut expr, &table_scope.alias_by_base);
    if !is_aggregation {
        expr = scan_policy_expr(expr, sources, context, &table_scope.alias_by_base)?;
    }
    Ok(expr)
}

fn unique_column_guard_from_constraint(
    constraint: &str,
    catalog: &TableCatalog,
) -> Option<Expr> {
    let expr = parse_expr(constraint).ok()?;
    let (table, column) = extract_simple_column_comparison(&expr)?;
    if !catalog.is_unique_column(&table, &column) {
        return None;
    }
    parse_expr(&format!("count(distinct {table}.{column}) = 1")).ok()
}

fn extract_simple_column_comparison(expr: &Expr) -> Option<(String, String)> {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq | BinaryOperator::NotEq,
            right,
        } => qualified_column_ref(left).or_else(|| qualified_column_ref(right)),
        _ => None,
    }
}

fn qualified_column_ref(expr: &Expr) -> Option<(String, String)> {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => Some((
            parts[0].value.clone(),
            parts[1].value.clone(),
        )),
        _ => None,
    }
}

fn build_pgn_over_filter_expr(
    sources: &[String],
    constraint: &str,
    sink_alias: &Option<String>,
    applicability: PolicyApplicability,
    context: &RewriteContext,
    table_scope: &TableScope,
) -> Result<Expr, RewriteError> {
    if applicability == PolicyApplicability::RequiredSourceMissing {
        return Ok(bool_literal(false));
    }
    let mut expr = parse_expr(constraint)?;
    if let Some(sink) = &context.sink {
        expr = replace_sink_columns(expr, sink, &context.sink_expr_by_column);
        expr = replace_sink_columns(expr, "_OUTPUT_", &context.sink_expr_by_column);
        if let Some(sink_alias) = sink_alias {
            expr = replace_sink_columns(expr, sink_alias, &context.sink_expr_by_column);
        }
    }
    rewrite_column_qualifiers(&mut expr, &table_scope.alias_by_base);
    let _ = sources;
    Ok(expr)
}

fn apply_update_resolution(
    assignments: &mut Vec<Assignment>,
    selection: &mut Option<Expr>,
    expr: Expr,
    on_fail: Resolution,
    description: Option<&str>,
) -> Result<(), RewriteError> {
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
            upsert_invalid_string_assignment(assignments, expr, description)?;
        }
        Resolution::Llm => {
            let expr = resolver_expr(expr)?;
            *selection = Some(match selection.take() {
                Some(existing) => and_expr(existing, expr),
                None => expr,
            });
        }
    }
    Ok(())
}

fn build_single_alias_compat_dfc_filter_expr(
    constraint: &str,
    sink_alias: &Option<String>,
    context: &RewriteContext,
    sources: &[String],
    base: &str,
    alias: &str,
    is_aggregation: bool,
) -> Result<Expr, RewriteError> {
    let alias_map = HashMap::from([(base.to_string(), alias.to_string())]);
    let mut expr = parse_expr(constraint)?;
    if let Some(sink) = &context.sink {
        expr = replace_sink_columns(expr, sink, &context.sink_expr_by_column);
        expr = replace_sink_columns(expr, "_OUTPUT_", &context.sink_expr_by_column);
        if let Some(sink_alias) = sink_alias {
            expr = replace_sink_columns(expr, sink_alias, &context.sink_expr_by_column);
        }
    }
    rewrite_column_qualifiers(&mut expr, &alias_map);
    if !is_aggregation {
        expr = scan_policy_expr(expr, sources, context, &alias_map)?;
    }
    Ok(expr)
}

fn build_invalidate_projection_expr(
    sources: &[String],
    constraint: &str,
    sink_alias: &Option<String>,
    applicability: PolicyApplicability,
    context: &RewriteContext,
    table_scope: &TableScope,
) -> Result<Expr, RewriteError> {
    if applicability == PolicyApplicability::RequiredSourceMissing {
        return Ok(bool_literal(false));
    }
    let mut expr = parse_expr(constraint)?;
    if let Some(sink) = &context.sink {
        expr = replace_sink_columns(expr, sink, &context.sink_expr_by_column);
        expr = replace_sink_columns(expr, "_OUTPUT_", &context.sink_expr_by_column);
        if let Some(sink_alias) = sink_alias {
            expr = replace_sink_columns(expr, sink_alias, &context.sink_expr_by_column);
        }
    }
    rewrite_column_qualifiers(&mut expr, &table_scope.alias_by_base);
    let _ = sources;
    Ok(expr)
}

fn apply_resolution(
    select: &mut Select,
    expr: Expr,
    resolution: Resolution,
    description: Option<&str>,
    is_aggregation: bool,
    projection_expr: Option<Expr>,
) -> Result<(), RewriteError> {
    let invalidate_expr = projection_expr.unwrap_or_else(|| expr.clone());
    match resolution {
        Resolution::Remove => add_filter(select, expr, is_aggregation),
        Resolution::Kill => add_filter(
            select,
            kill_expr_for_select(expr, select, is_aggregation)?,
            is_aggregation,
        ),
        Resolution::Invalidate => {
            upsert_select_projection(select, "valid", |existing| {
                Ok::<_, RewriteError>(
                    existing.map_or(invalidate_expr.clone(), |existing| {
                        and_expr(existing, invalidate_expr.clone())
                    }),
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

fn kill_expr_for_select(
    expr: Expr,
    select: &Select,
    is_aggregation: bool,
) -> Result<Expr, RewriteError> {
    if !is_aggregation {
        return kill_expr(expr);
    }
    let tautology = aggregation_kill_tautology(select)?;
    parse_expr(&format!(
        "CASE WHEN {expr} THEN ({tautology}) OR kill() ELSE true END"
    ))
}

fn aggregation_kill_tautology(select: &Select) -> Result<Expr, RewriteError> {
    let source_prefix = select
        .from
        .first()
        .and_then(|table| table_factor_base_and_alias(&table.relation))
        .map(|(base, _)| base);
    if let GroupByExpr::Expressions(exprs, _) = &select.group_by
        && let Some(group_expr) = exprs.first() {
            let tautology = qualify_kill_tautology_expr(group_expr, source_prefix.as_deref());
            return parse_expr(&format!("{tautology} = {tautology}"));
        }
    for item in &select.projection {
        if let SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } = item
            && !expr_contains_aggregate(expr) {
                let tautology = qualify_kill_tautology_expr(expr, source_prefix.as_deref());
                return parse_expr(&format!("{tautology} = {tautology}"));
            }
    }
    parse_expr("true")
}

fn qualify_kill_tautology_expr(expr: &Expr, source_prefix: Option<&str>) -> String {
    if let Some(prefix) = source_prefix
        && let Expr::Identifier(ident) = expr {
            return format!("{prefix}.{}", ident.value);
        }
    expr.to_string()
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

fn aggregate_temp_column(index: usize) -> String {
    format!("__passant_agg_{}", index.saturating_sub(1))
}

fn transform_scan_aggregates(expr: Expr) -> Result<Expr, RewriteError> {
    if let Some(rewritten) = rewrite_count_distinct_equality(&expr)? {
        return Ok(rewritten);
    }
    Ok(transform_scan_aggregates_recursive(expr))
}

fn rewrite_count_distinct_equality(expr: &Expr) -> Result<Option<Expr>, RewriteError> {
    let Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    } = expr
    else {
        return Ok(None);
    };
    let Some(col) = count_distinct_inner_column(left) else {
        return Ok(None);
    };
    let Expr::Value(value) = &**right else {
        return Ok(None);
    };
    if value.to_string() != "1" {
        return Ok(None);
    }
    Ok(Some(parse_expr(&format!("{col} IS NOT NULL"))?))
}

fn count_distinct_inner_column(expr: &Expr) -> Option<String> {
    let Expr::Function(function) = expr else {
        return None;
    };
    if !function.name.to_string().eq_ignore_ascii_case("count") || !function_is_distinct(function) {
        return None;
    }
    first_function_expr(function).map(|expr| expr.to_string())
}

fn transform_scan_aggregates_recursive(expr: Expr) -> Expr {
    match expr {
        Expr::Function(function) => transform_scan_aggregate_function(function),
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(transform_scan_aggregates_recursive(*left)),
            op,
            right: Box::new(transform_scan_aggregates_recursive(*right)),
        },
        Expr::Nested(inner) => Expr::Nested(Box::new(transform_scan_aggregates_recursive(*inner))),
        Expr::UnaryOp { op, expr } => Expr::UnaryOp {
            op,
            expr: Box::new(transform_scan_aggregates_recursive(*expr)),
        },
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => Expr::Case {
            operand: operand.map(|expr| Box::new(transform_scan_aggregates_recursive(*expr))),
            conditions: conditions
                .into_iter()
                .map(transform_scan_aggregates_recursive)
                .collect(),
            results: results
                .into_iter()
                .map(transform_scan_aggregates_recursive)
                .collect(),
            else_result: else_result.map(|expr| Box::new(transform_scan_aggregates_recursive(*expr))),
        },
        other => other,
    }
}

fn transform_scan_aggregate_function(function: sqlparser::ast::Function) -> Expr {
    let name = function.name.to_string();
    let lower = name.to_ascii_lowercase();
    if matches!(lower.as_str(), "count_if" | "countif") {
        if let Some(condition) = first_function_expr(&function) {
            return parse_expr_or_identity(&format!(
                "CASE WHEN {condition} THEN 1 ELSE 0 END"
            ));
        }
        return parse_expr_or_identity("0");
    }
    if lower == "array_agg" {
        if let Some(column) = first_function_expr(&function) {
            return parse_expr_or_identity(&format!("[{column}]"));
        }
        return parse_expr_or_identity("[NULL]");
    }
    if lower == "count" && function_is_distinct(&function)
        && let Some(column) = first_function_expr(&function) {
            return column;
        }
    if is_count_like_aggregate(&lower, &function) {
        return parse_expr_or_identity("1");
    }
    if is_aggregate_name(&name) {
        return first_function_expr(&function).unwrap_or(Expr::Function(function));
    }
    Expr::Function(function)
}

fn is_count_like_aggregate(name: &str, function: &sqlparser::ast::Function) -> bool {
    matches!(
        name,
        "count" | "count_star" | "approx_count_distinct" | "approx_distinct" | "regr_count"
    ) || (name == "count" && function_is_distinct(function))
}

fn function_is_distinct(function: &sqlparser::ast::Function) -> bool {
    match &function.args {
        FunctionArguments::List(list) => {
            list.duplicate_treatment == Some(DuplicateTreatment::Distinct)
        }
        _ => false,
    }
}

fn is_scan_transformable_non_distributive(aggregate: &str) -> bool {
    let lower = aggregate.to_ascii_lowercase();
    lower.contains("array_agg") || lower.contains("count_if") || lower.contains("countif")
}

fn parse_expr_or_identity(sql: &str) -> Expr {
    parse_expr(sql).unwrap_or_else(|_| parse_expr("true").expect("true should parse"))
}

fn restore_inner_join_keyword(original: &str, rewritten: String) -> String {
    if !original.to_ascii_uppercase().contains(" INNER JOIN ") {
        return rewritten;
    }
    let upper = rewritten.to_ascii_uppercase();
    if upper.contains(" INNER JOIN ") {
        return rewritten;
    }
    rewritten.replace(" JOIN ", " INNER JOIN ")
}

fn normalize_distinct_casing(mut sql: String) -> String {
    while let Some(index) = sql.find("count(DISTINCT") {
        sql.replace_range(index..index + "count(DISTINCT".len(), "count(distinct");
    }
    sql
}

fn restore_preserved_aggregate_sql(mut sql: String, policies: &[PolicyIr]) -> String {
    for policy in policies {
        let PolicyIr::CompatAggregate(policy) = policy else {
            continue;
        };
        let Ok(aggregates) = constraint_aggregates(
            &policy.constraint,
            &policy.sources,
            policy.sink.as_deref(),
        ) else {
            continue;
        };
        for aggregate in aggregates {
            let Ok(normalized) = parse_expr(&aggregate.sql) else {
                continue;
            };
            let normalized_sql = normalized.to_string();
            if normalized_sql != aggregate.sql && sql.contains(&normalized_sql) {
                sql = sql.replace(&normalized_sql, &aggregate.sql);
            }
        }
    }
    sql
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
    constraint_aggregates(constraint, sources, None)
}

fn constraint_aggregates(
    constraint: &str,
    sources: &[String],
    sink: Option<&str>,
) -> Result<Vec<SourceAggregate>, RewriteError> {
    let expr = parse_expr(constraint)?;
    let mut aggregates = Vec::new();
    collect_constraint_aggregates(&expr, constraint, sources, sink, &mut aggregates);
    Ok(aggregates)
}

fn policy_aggregate_temp_entries(
    constraint: &str,
    sources: &[String],
    sink: Option<&str>,
) -> Result<Vec<SourceAggregate>, RewriteError> {
    let all = constraint_aggregates(constraint, sources, sink)?;
    let mut ordered = Vec::new();
  for source in sources {
        for aggregate in &all {
            if !aggregate.is_sink_aggregate
                && expr_references_table(&aggregate.expr, source)
                && ordered.iter().all(|existing: &SourceAggregate| existing.sql != aggregate.sql)
            {
                ordered.push(aggregate.clone());
            }
        }
    }
    for aggregate in &all {
        if !aggregate.is_sink_aggregate
            && ordered.iter().all(|existing: &SourceAggregate| existing.sql != aggregate.sql)
        {
            ordered.push(aggregate.clone());
        }
    }
    for aggregate in &all {
        if aggregate.is_sink_aggregate
            && ordered.iter().all(|existing: &SourceAggregate| existing.sql != aggregate.sql)
        {
            ordered.push(aggregate.clone());
        }
    }
    Ok(ordered)
}

fn aggregate_sql_from_constraint(constraint: &str, expr: &Expr) -> String {
    let rendered = expr.to_string();
    if constraint.contains(&rendered) {
        return rendered;
    }
    let lower = constraint.to_ascii_lowercase();
    let needle = rendered.to_ascii_lowercase();
    if let Some(start) = lower.find(&needle) {
        return constraint[start..start + rendered.len()].to_string();
    }
    rendered
}

fn collect_constraint_aggregates(
    expr: &Expr,
    constraint: &str,
    sources: &[String],
    sink: Option<&str>,
    aggregates: &mut Vec<SourceAggregate>,
) {
    match expr {
        Expr::Function(function) if is_aggregate_name(&function.name.to_string()) => {
            if let Some(input) = first_function_expr(function) {
                let refs_source = expr_references_any_source(&input, sources)
                    || expr_references_any_source(expr, sources);
                let refs_sink = sink.is_some_and(|sink| expr_references_table(expr, sink));
                if refs_source || refs_sink {
                    aggregates.push(SourceAggregate {
                        sql: aggregate_sql_from_constraint(constraint, expr),
                        function_name: function.name.to_string(),
                        expr: expr.clone(),
                        is_sink_aggregate: refs_sink && !refs_source,
                    });
                }
            }
        }
        Expr::Function(function) => {
            if let FunctionArguments::List(args) = &function.args {
                for arg in &args.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                    | FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(expr),
                        ..
                    }
                    | FunctionArg::ExprNamed {
                        arg: FunctionArgExpr::Expr(expr),
                        ..
                    } = arg
                    {
                        collect_constraint_aggregates(expr, constraint, sources, sink, aggregates);
                    }
                }
            }
            if let Some(filter) = function.filter.as_ref() {
                collect_constraint_aggregates(filter, constraint, sources, sink, aggregates);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_constraint_aggregates(left, constraint, sources, sink, aggregates);
            collect_constraint_aggregates(right, constraint, sources, sink, aggregates);
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => {
            collect_constraint_aggregates(expr, constraint, sources, sink, aggregates)
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                collect_constraint_aggregates(operand, constraint, sources, sink, aggregates);
            }
            for expr in conditions.iter().chain(results.iter()) {
                collect_constraint_aggregates(expr, constraint, sources, sink, aggregates);
            }
            if let Some(else_result) = else_result {
                collect_constraint_aggregates(else_result, constraint, sources, sink, aggregates);
            }
        }
        _ => {}
    }
}

fn expr_references_table(expr: &Expr, table: &str) -> bool {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            parts[0].value.eq_ignore_ascii_case(table)
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_references_table(left, table) || expr_references_table(right, table)
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => expr_references_table(expr, table),
        Expr::Function(function) => {
            if let FunctionArguments::List(args) = &function.args
                && args.args.iter().any(|arg| match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                    | FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(expr),
                        ..
                    }
                    | FunctionArg::ExprNamed {
                        arg: FunctionArgExpr::Expr(expr),
                        ..
                    } => expr_references_table(expr, table),
                    _ => false,
                }) {
                    return true;
                }
            function
                .filter
                .as_deref()
                .is_some_and(|filter| expr_references_table(filter, table))
        }
        _ => false,
    }
}

fn aggregate_has_filter(expr: &Expr) -> bool {
    matches!(expr, Expr::Function(function) if function.filter.is_some())
}

fn expr_is_aggregate(expr: &Expr) -> bool {
    matches!(expr, Expr::Function(function) if is_aggregate_name(&function.name.to_string()))
}

fn aggregate_temp_projection_expr(
    aggregate: &SourceAggregate,
    is_query_aggregation: bool,
    context: Option<&RewriteContext>,
    sink: Option<&str>,
) -> Result<Expr, RewriteError> {
    let preserve_full_aggregate =
        is_query_aggregation || aggregate_has_filter(&aggregate.expr);

    let expr = if aggregate_has_filter(&aggregate.expr) {
        parse_expr(&aggregate.sql)?
    } else if aggregate.is_sink_aggregate
        && let (Some(context), Some(sink)) = (context, sink)
    {
        if let Expr::Function(function) = &aggregate.expr
            && let Some(inner) = first_function_expr(function)
        {
            let mapped_inner = replace_sink_columns(inner, sink, &context.sink_expr_by_column);
            let mapped_inner =
                replace_sink_columns(mapped_inner, "_OUTPUT_", &context.sink_expr_by_column);
            if preserve_full_aggregate && expr_is_aggregate(&mapped_inner) {
                mapped_inner
            } else if preserve_full_aggregate {
                replace_sink_columns(
                    replace_sink_columns(
                        parse_expr(&aggregate.sql)?,
                        sink,
                        &context.sink_expr_by_column,
                    ),
                    "_OUTPUT_",
                    &context.sink_expr_by_column,
                )
            } else {
                mapped_inner
            }
        } else if preserve_full_aggregate {
            replace_sink_columns(
                replace_sink_columns(
                    parse_expr(&aggregate.sql)?,
                    sink,
                    &context.sink_expr_by_column,
                ),
                "_OUTPUT_",
                &context.sink_expr_by_column,
            )
        } else {
            parse_expr(&aggregate.sql)?
        }
    } else if preserve_full_aggregate {
        parse_expr(&aggregate.sql)?
    } else if let Expr::Function(function) = &aggregate.expr
        && let Some(inner) = first_function_expr(function)
    {
        inner.clone()
    } else {
        parse_expr(&aggregate.sql)?
    };
    Ok(expr)
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

fn join_conjuncts(mut conjuncts: Vec<Expr>) -> Expr {
    let first = conjuncts.remove(0);
    conjuncts.into_iter().fold(first, and_expr)
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
    aliases_by_base: HashMap<String, Vec<String>>,
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
                    self.alias_by_base
                        .insert(key.clone(), alias.name.value.clone());
                    self.aliases_by_base
                        .entry(key)
                        .or_default()
                        .push(alias.name.value.clone());
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
