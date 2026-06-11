//! Inject policy DIMENSION relations into SELECT scopes for evaluation.

use std::collections::{HashMap, HashSet};

use sqlparser::ast::{
    Expr, Join, JoinConstraint, JoinOperator, Select, Statement, TableFactor, TableWithJoins,
};

use crate::catalog::TableCatalog;
use crate::diagnostics::RewriteError;
use crate::identifiers::{TableKey, TableName};
use crate::parser::parse_query;
use crate::policy::PolicyIr;
use crate::rewriter::constraint_preprocess::preprocess_policy_constraint;
use crate::rewriter::expr::first_function_expr;
use crate::rewriter::scope::TableScope;
use crate::sql::builders::{object_name, table_alias};
use crate::sql::parse_projection_expr;

fn expansion_warning(table: &str) -> String {
    format!(
        "Dimension '{table}' is joined for policy evaluation; query output multiplicity may \
         increase if dimension cardinality or key relationships change"
    )
}

/// Cross-join missing dimension tables and inline dimension subqueries into `select.from`.
///
/// Returns dimension table names (or subquery aliases) that were not joined into the query.
pub(crate) fn inject_policy_dimensions(
    select: &mut Select,
    policy: &PolicyIr,
    catalog: &TableCatalog,
    warnings: &mut Vec<String>,
    cached_join_plan: Option<&DimensionJoinPlan>,
) -> Result<Vec<String>, RewriteError> {
    let PolicyIr::Pgn {
        sources,
        dimension_tables,
        dimension_aliases,
        dimension_queries,
        constraint,
        ..
    } = policy;

    if dimension_tables.is_empty() && dimension_queries.is_empty() {
        return Ok(Vec::new());
    }

    let mut skipped = Vec::new();
    let source_keys: HashSet<TableKey> = sources.iter().map(|s| TableKey::new(s)).collect();
    let (source_join_conditions, dimension_edges) = if let Some(plan) = cached_join_plan {
        (plan.source_conditions.clone(), plan.dimension_edges.clone())
    } else {
        dimension_join_graph(
            constraint,
            dimension_tables,
            dimension_aliases,
            dimension_queries,
            &source_keys,
        )?
    };
    let mut scope = TableScope::from_select(select);
    let mut joined_keys: HashSet<TableKey> = scope.base_tables.clone();
    for alias in scope.alias_by_base.as_map().values() {
        joined_keys.insert(TableKey::new(alias));
    }
    for source in sources {
        joined_keys.insert(TableKey::new(source));
    }

    let mut pending: Vec<(String, Option<&str>)> = dimension_tables
        .iter()
        .filter(|table| !dimension_table_in_scope(table, dimension_aliases, &scope))
        .map(|table| {
            let alias = dimension_aliases
                .iter()
                .find(|(_, base)| base.eq_ignore_ascii_case(table))
                .map(|(alias, _)| alias.as_str());
            (table.clone(), alias)
        })
        .collect();

    while !pending.is_empty() {
        let mut progressed = false;
        let mut next_pending = Vec::new();
        for (table, alias) in pending {
            if dimension_table_in_scope(&table, dimension_aliases, &scope) {
                progressed = true;
                continue;
            }
            let table_key = alias
                .map(TableKey::new)
                .unwrap_or_else(|| TableKey::new(&table));
            let factor = table_factor_for_dimension(&table, alias);
            let on = source_join_conditions.get(&table_key).cloned().or_else(|| {
                dimension_edges.iter().find_map(|edge| {
                    if edge.left == table_key && joined_keys.contains(&edge.right) {
                        Some(edge.on.clone())
                    } else if edge.right == table_key && joined_keys.contains(&edge.left) {
                        Some(reverse_equality(&edge.on))
                    } else {
                        None
                    }
                })
            });
            if let Some(join) =
                plan_dimension_join(&table, alias, on.as_ref(), catalog, dimension_aliases)
            {
                warnings.extend(join.warnings);
                append_dimension_join(select, factor, join.on)?;
                joined_keys.insert(table_key);
                scope = TableScope::from_select(select);
                progressed = true;
            } else {
                next_pending.push((table, alias));
            }
        }
        if progressed {
            pending = next_pending;
            continue;
        }
        for (table, alias) in next_pending {
            if dimension_table_in_scope(&table, dimension_aliases, &scope) {
                continue;
            }
            let table_key = alias
                .map(TableKey::new)
                .unwrap_or_else(|| TableKey::new(&table));
            let factor = table_factor_for_dimension(&table, alias);
            if let Some(join) = plan_dimension_join(&table, alias, None, catalog, dimension_aliases)
            {
                warnings.extend(join.warnings);
                append_dimension_join(select, factor, join.on)?;
                joined_keys.insert(table_key);
                scope = TableScope::from_select(select);
            } else {
                warnings.push(format!(
                    "Dimension '{table}' was not joined: no equality link to the query or \
                     another dimension in the policy constraint and catalog row count is not 1"
                ));
                skipped.push(table);
            }
        }
        break;
    }

    for (alias, query_sql) in dimension_queries {
        if joined_keys.contains(&TableKey::new(alias)) {
            continue;
        }
        let factor = derived_dimension_factor(query_sql, alias)?;
        let table_key = TableKey::new(alias);
        let on = source_join_conditions.get(&table_key).cloned().or_else(|| {
            dimension_edges.iter().find_map(|edge| {
                if edge.left == table_key && joined_keys.contains(&edge.right) {
                    Some(edge.on.clone())
                } else if edge.right == table_key && joined_keys.contains(&edge.left) {
                    Some(reverse_equality(&edge.on))
                } else {
                    None
                }
            })
        });
        let table = alias.as_str();
        if let Some(join) =
            plan_dimension_join(table, Some(alias), on.as_ref(), catalog, dimension_aliases)
        {
            warnings.extend(join.warnings);
            append_dimension_join(select, factor, join.on)?;
            joined_keys.insert(table_key);
        } else if let Some(join) =
            plan_dimension_join(table, Some(alias), None, catalog, dimension_aliases)
        {
            warnings.extend(join.warnings);
            append_dimension_join(select, factor, join.on)?;
            joined_keys.insert(table_key);
        } else {
            warnings.push(expansion_warning(alias));
            append_dimension_join(select, factor, None)?;
            joined_keys.insert(table_key);
        }
    }

    Ok(skipped)
}

/// True when the constraint references a dimension table or alias that was not joined.
pub(crate) fn constraint_references_skipped_dimensions(
    constraint: &str,
    skipped: &[String],
    dimension_aliases: &HashMap<String, String>,
) -> bool {
    if skipped.is_empty() {
        return false;
    }
    let mut keys = HashSet::new();
    for name in skipped {
        keys.insert(TableKey::new(name));
        for (alias, base) in dimension_aliases {
            if base.eq_ignore_ascii_case(name) {
                keys.insert(TableKey::new(alias));
            }
        }
    }
    let constraint = preprocess_policy_constraint(constraint);
    let Ok(expr) = parse_projection_expr(&constraint) else {
        return true;
    };
    expr_references_dimension_keys(&expr, &keys)
}

fn expr_references_dimension_keys(expr: &Expr, dimension_keys: &HashSet<TableKey>) -> bool {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            dimension_keys.contains(&TableKey::new(parts[0].value.as_str()))
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_references_dimension_keys(left, dimension_keys)
                || expr_references_dimension_keys(right, dimension_keys)
        }
        Expr::Nested(inner)
        | Expr::UnaryOp { expr: inner, .. }
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner) => expr_references_dimension_keys(inner, dimension_keys),
        Expr::Function(function) => {
            if let sqlparser::ast::FunctionArguments::List(list) = &function.args {
                list.args.iter().any(|arg| {
                    matches!(
                        arg,
                        sqlparser::ast::FunctionArg::Unnamed(
                            sqlparser::ast::FunctionArgExpr::Expr(arg_expr)
                        ) | sqlparser::ast::FunctionArg::Named {
                            arg: sqlparser::ast::FunctionArgExpr::Expr(arg_expr),
                            ..
                        } | sqlparser::ast::FunctionArg::ExprNamed {
                            arg: sqlparser::ast::FunctionArgExpr::Expr(arg_expr),
                            ..
                        } if expr_references_dimension_keys(arg_expr, dimension_keys)
                    )
                }) || function
                    .filter
                    .as_ref()
                    .is_some_and(|filter| expr_references_dimension_keys(filter, dimension_keys))
            } else {
                false
            }
        }
        _ => false,
    }
}

struct PlannedDimensionJoin {
    on: Option<Expr>,
    warnings: Vec<String>,
}

fn plan_dimension_join(
    dimension_base: &str,
    alias: Option<&str>,
    on: Option<&Expr>,
    catalog: &TableCatalog,
    dimension_aliases: &HashMap<String, String>,
) -> Option<PlannedDimensionJoin> {
    let lookup_table = alias.unwrap_or(dimension_base);
    let mut warnings = Vec::new();

    if let Some(on_expr) = on {
        if !dimension_join_references_unique_key(
            on_expr,
            dimension_base,
            dimension_aliases,
            catalog,
        ) {
            warnings.push(format!(
                "Dimension '{dimension_base}' join uses constraint equality but the dimension \
                 key column is not marked unique in the catalog; rows may duplicate if the \
                 relationship is not one-to-one"
            ));
        }
        warnings.push(expansion_warning(dimension_base));
        return Some(PlannedDimensionJoin {
            on: Some(on_expr.clone()),
            warnings,
        });
    }

    if catalog.is_singleton_table(lookup_table) || catalog.is_singleton_table(dimension_base) {
        warnings.push(expansion_warning(dimension_base));
        return Some(PlannedDimensionJoin { on: None, warnings });
    }

    None
}

fn dimension_join_references_unique_key(
    on: &Expr,
    dimension_base: &str,
    dimension_aliases: &HashMap<String, String>,
    catalog: &TableCatalog,
) -> bool {
    let Expr::BinaryOp {
        left,
        op: sqlparser::ast::BinaryOperator::Eq,
        right,
    } = on
    else {
        return false;
    };
    dimension_side_is_unique_key(left, dimension_base, dimension_aliases, catalog)
        || dimension_side_is_unique_key(right, dimension_base, dimension_aliases, catalog)
}

fn dimension_side_is_unique_key(
    expr: &Expr,
    dimension_base: &str,
    dimension_aliases: &HashMap<String, String>,
    catalog: &TableCatalog,
) -> bool {
    let Expr::CompoundIdentifier(parts) = expr else {
        return false;
    };
    if parts.len() < 2 {
        return false;
    }
    let table = parts[0].value.as_str();
    let column = parts[1].value.as_str();
    let base = dimension_aliases
        .get(table)
        .map(String::as_str)
        .unwrap_or(table);
    if !base.eq_ignore_ascii_case(dimension_base) {
        return false;
    }
    catalog.is_unique_column(base, column)
}

fn dimension_table_in_scope(
    table: &str,
    dimension_aliases: &HashMap<String, String>,
    scope: &TableScope,
) -> bool {
    let base = TableName::parse(table);
    if scope.base_tables.contains(&TableKey::from_table(&base)) {
        return true;
    }
    if scope.alias_by_base.get(&base).is_some() {
        return true;
    }
    for (alias, base_table) in dimension_aliases {
        if !base_table.eq_ignore_ascii_case(table) {
            continue;
        }
        if scope.base_tables.contains(&TableKey::new(alias)) {
            return true;
        }
    }
    false
}

fn table_factor_for_dimension(table: &str, alias: Option<&str>) -> TableFactor {
    TableFactor::Table {
        name: object_name(table),
        alias: alias.map(table_alias),
        args: None,
        with_hints: Vec::new(),
        version: None,
        with_ordinality: false,
        partitions: Vec::new(),
        json_path: None,
    }
}

fn derived_dimension_factor(query_sql: &str, alias: &str) -> Result<TableFactor, RewriteError> {
    let statement = parse_query(query_sql)
        .map_err(|err| RewriteError::unsupported_statement(format!("dimension subquery: {err}")))?;
    let Statement::Query(query) = statement else {
        return Err(RewriteError::unsupported_statement(
            "dimension subquery must be a query",
        ));
    };
    Ok(TableFactor::Derived {
        lateral: false,
        subquery: query,
        alias: Some(table_alias(alias)),
    })
}

#[derive(Debug, Clone)]
pub(crate) struct DimensionJoinEdge {
    left: TableKey,
    right: TableKey,
    on: Expr,
}

/// Equalities extracted from a policy constraint for dimension injection (cached at registration).
#[derive(Debug, Clone)]
pub(crate) struct DimensionJoinPlan {
    pub source_conditions: HashMap<TableKey, Expr>,
    pub dimension_edges: Vec<DimensionJoinEdge>,
}

pub(crate) fn compile_dimension_join_plan(
    constraint_ast: &Expr,
    dimension_tables: &[String],
    dimension_aliases: &HashMap<String, String>,
    dimension_queries: &HashMap<String, String>,
    source_keys: &HashSet<TableKey>,
) -> Option<DimensionJoinPlan> {
    if dimension_tables.is_empty() && dimension_queries.is_empty() {
        return None;
    }
    let mut dimension_keys = HashSet::new();
    for table in dimension_tables {
        dimension_keys.insert(TableKey::new(table));
        for (alias, base) in dimension_aliases {
            if base.eq_ignore_ascii_case(table) {
                dimension_keys.insert(TableKey::new(alias));
            }
        }
    }
    for alias in dimension_queries.keys() {
        dimension_keys.insert(TableKey::new(alias));
    }
    let mut source_conditions = HashMap::new();
    let mut dimension_edges = Vec::new();
    collect_dimension_join_graph(
        constraint_ast,
        &dimension_keys,
        source_keys,
        &mut source_conditions,
        &mut dimension_edges,
    );
    Some(DimensionJoinPlan {
        source_conditions,
        dimension_edges,
    })
}

fn reverse_equality(on: &Expr) -> Expr {
    let Expr::BinaryOp {
        left,
        op: sqlparser::ast::BinaryOperator::Eq,
        right,
    } = on
    else {
        return on.clone();
    };
    Expr::BinaryOp {
        left: right.clone(),
        op: sqlparser::ast::BinaryOperator::Eq,
        right: left.clone(),
    }
}

fn dimension_join_graph(
    constraint: &str,
    dimension_tables: &[String],
    dimension_aliases: &HashMap<String, String>,
    dimension_queries: &HashMap<String, String>,
    source_keys: &HashSet<TableKey>,
) -> Result<(HashMap<TableKey, Expr>, Vec<DimensionJoinEdge>), RewriteError> {
    let constraint = preprocess_policy_constraint(constraint);
    let expr = parse_projection_expr(&constraint).map_err(|err| {
        RewriteError::unsupported_statement(format!("dimension join analysis: {err}"))
    })?;
    let plan = compile_dimension_join_plan(
        &expr,
        dimension_tables,
        dimension_aliases,
        dimension_queries,
        source_keys,
    )
    .unwrap_or(DimensionJoinPlan {
        source_conditions: HashMap::new(),
        dimension_edges: Vec::new(),
    });
    Ok((plan.source_conditions, plan.dimension_edges))
}

fn table_key_from_qualified(expr: &Expr) -> Option<TableKey> {
    let Expr::CompoundIdentifier(parts) = expr else {
        return None;
    };
    if parts.len() >= 2 {
        Some(TableKey::new(parts[0].value.as_str()))
    } else {
        None
    }
}

fn collect_dimension_join_graph(
    expr: &Expr,
    dimension_keys: &HashSet<TableKey>,
    source_keys: &HashSet<TableKey>,
    source_conditions: &mut HashMap<TableKey, Expr>,
    dimension_edges: &mut Vec<DimensionJoinEdge>,
) {
    match expr {
        Expr::BinaryOp {
            left,
            op: sqlparser::ast::BinaryOperator::Eq,
            right,
        } => {
            if let Some((dim_key, on)) =
                dimension_equality_pair(left, right, dimension_keys, source_keys)
            {
                source_conditions.entry(dim_key).or_insert(on);
            } else if let (Some(left_key), Some(right_key)) = (
                table_key_from_qualified(left),
                table_key_from_qualified(right),
            ) && dimension_keys.contains(&left_key)
                && dimension_keys.contains(&right_key)
            {
                dimension_edges.push(DimensionJoinEdge {
                    left: left_key,
                    right: right_key,
                    on: Expr::BinaryOp {
                        left: left.clone(),
                        op: sqlparser::ast::BinaryOperator::Eq,
                        right: right.clone(),
                    },
                });
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_dimension_join_graph(
                left,
                dimension_keys,
                source_keys,
                source_conditions,
                dimension_edges,
            );
            collect_dimension_join_graph(
                right,
                dimension_keys,
                source_keys,
                source_conditions,
                dimension_edges,
            );
        }
        Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Not,
            ..
        } => {}
        Expr::Nested(inner)
        | Expr::UnaryOp { expr: inner, .. }
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner) => {
            collect_dimension_join_graph(
                inner,
                dimension_keys,
                source_keys,
                source_conditions,
                dimension_edges,
            );
        }
        Expr::Function(function) => {
            if let sqlparser::ast::FunctionArguments::List(list) = &function.args {
                for arg in &list.args {
                    if let sqlparser::ast::FunctionArg::Unnamed(
                        sqlparser::ast::FunctionArgExpr::Expr(arg_expr),
                    )
                    | sqlparser::ast::FunctionArg::Named {
                        arg: sqlparser::ast::FunctionArgExpr::Expr(arg_expr),
                        ..
                    }
                    | sqlparser::ast::FunctionArg::ExprNamed {
                        arg: sqlparser::ast::FunctionArgExpr::Expr(arg_expr),
                        ..
                    } = arg
                    {
                        collect_dimension_join_graph(
                            arg_expr,
                            dimension_keys,
                            source_keys,
                            source_conditions,
                            dimension_edges,
                        );
                    }
                }
            }
            if let Some(filter) = function.filter.as_ref() {
                collect_dimension_join_graph(
                    filter,
                    dimension_keys,
                    source_keys,
                    source_conditions,
                    dimension_edges,
                );
            }
        }
        _ => {}
    }
}

fn dimension_equality_side(
    expr: &Expr,
    dimension_keys: &HashSet<TableKey>,
) -> Option<(TableKey, Expr)> {
    let Expr::CompoundIdentifier(parts) = expr else {
        return None;
    };
    if parts.len() < 2 {
        return None;
    }
    let table_key = TableKey::new(parts[0].value.as_str());
    if dimension_keys.contains(&table_key) {
        return Some((table_key, expr.clone()));
    }
    None
}

fn source_join_equivalence_expr(expr: &Expr, source_keys: &HashSet<TableKey>) -> Option<Expr> {
    if let Expr::CompoundIdentifier(parts) = expr {
        if parts.len() >= 2 && source_keys.contains(&TableKey::new(parts[0].value.as_str())) {
            return Some(expr.clone());
        }
        return None;
    }
    let Expr::Function(function) = expr else {
        return None;
    };
    let name = function.name.to_string().to_ascii_lowercase();
    if !matches!(name.as_str(), "max" | "min") {
        return None;
    }
    let arg = first_function_expr(function)?;
    if let Expr::CompoundIdentifier(parts) = &arg
        && parts.len() >= 2
        && source_keys.contains(&TableKey::new(parts[0].value.as_str()))
    {
        return Some(arg);
    }
    None
}

fn dimension_equality_pair(
    left: &Expr,
    right: &Expr,
    dimension_keys: &HashSet<TableKey>,
    source_keys: &HashSet<TableKey>,
) -> Option<(TableKey, Expr)> {
    if let Some((dim_key, dim_side)) = dimension_equality_side(left, dimension_keys)
        && let Some(source_side) = source_join_equivalence_expr(right, source_keys)
    {
        return Some((
            dim_key,
            Expr::BinaryOp {
                left: Box::new(dim_side),
                op: sqlparser::ast::BinaryOperator::Eq,
                right: Box::new(source_side),
            },
        ));
    }
    if let Some((dim_key, dim_side)) = dimension_equality_side(right, dimension_keys)
        && let Some(source_side) = source_join_equivalence_expr(left, source_keys)
    {
        return Some((
            dim_key,
            Expr::BinaryOp {
                left: Box::new(dim_side),
                op: sqlparser::ast::BinaryOperator::Eq,
                right: Box::new(source_side),
            },
        ));
    }
    None
}

fn append_dimension_join(
    select: &mut Select,
    factor: TableFactor,
    on: Option<Expr>,
) -> Result<(), RewriteError> {
    let join_operator = match on {
        Some(on) => JoinOperator::Inner(JoinConstraint::On(on)),
        None => JoinOperator::CrossJoin,
    };
    if select.from.is_empty() {
        select.from.push(TableWithJoins {
            relation: factor,
            joins: Vec::new(),
        });
        return Ok(());
    }
    let table = select
        .from
        .last_mut()
        .ok_or_else(|| RewriteError::unsupported_statement("SELECT without FROM"))?;
    table.joins.push(Join {
        relation: factor,
        global: false,
        join_operator,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use sqlparser::ast::SetExpr;
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;

    use super::*;
    use crate::policy::{PolicyIr, Resolution};

    fn parse_select(sql: &str) -> Select {
        let statement = Parser::parse_sql(&GenericDialect {}, sql)
            .expect("parse")
            .pop()
            .expect("statement");
        let Statement::Query(query) = statement else {
            panic!("expected query");
        };
        let SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        *select
    }

    fn policy_with_dimensions(
        tables: &[&str],
        aliases: &[(&str, &str)],
        queries: &[(&str, &str)],
        constraint: &str,
    ) -> PolicyIr {
        PolicyIr::Pgn {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimension_tables: tables.iter().map(|t| (*t).to_string()).collect(),
            dimension_aliases: aliases
                .iter()
                .map(|(alias, base)| (alias.to_string(), base.to_string()))
                .collect(),
            dimension_queries: queries
                .iter()
                .map(|(alias, query)| (alias.to_string(), query.to_string()))
                .collect(),
            sink: None,
            sink_alias: None,
            source_aliases: HashMap::new(),
            constraint: constraint.to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }
    }

    #[test]
    fn injects_singleton_dimension_cross_join() {
        let mut catalog = TableCatalog::new();
        catalog.register_table_row_count("session_user", 1);
        let mut select = parse_select("SELECT foo.id FROM foo");
        let policy = policy_with_dimensions(
            &["session_user"],
            &[("u", "session_user")],
            &[],
            "max(foo.id) > 0",
        );
        let mut warnings = Vec::new();
        inject_policy_dimensions(&mut select, &policy, &catalog, &mut warnings, None)
            .expect("inject");
        let sql = select.to_string();
        assert!(sql.contains("CROSS JOIN"));
        assert!(sql.contains("session_user"));
        assert!(warnings.iter().any(|w| w.contains("multiplicity")));
    }

    #[test]
    fn skips_unsafe_dimension_without_join_key_or_singleton() {
        let catalog = TableCatalog::new();
        let mut select = parse_select("SELECT foo.id FROM foo");
        let policy = policy_with_dimensions(
            &["regions"],
            &[],
            &[],
            "max(foo.id) > 0 AND regions.code = 'US'",
        );
        let mut warnings = Vec::new();
        inject_policy_dimensions(&mut select, &policy, &catalog, &mut warnings, None)
            .expect("inject");
        let sql = select.to_string();
        assert!(!sql.contains("regions"));
        assert!(warnings.iter().any(|w| w.contains("was not joined")));
    }

    #[test]
    fn inner_join_when_constraint_links_source_and_dimension() {
        let mut catalog = TableCatalog::new();
        catalog.register_unique_column("regions", "id");
        let mut select = parse_select("SELECT foo.id FROM foo");
        let policy = policy_with_dimensions(
            &["regions"],
            &[],
            &[],
            "max(foo.id) > 0 AND foo.region_id = regions.id AND regions.code = 'US'",
        );
        let mut warnings = Vec::new();
        inject_policy_dimensions(&mut select, &policy, &catalog, &mut warnings, None)
            .expect("inject");
        let sql = select.to_string();
        assert!(sql.contains("INNER JOIN") || sql.contains("JOIN regions"));
        assert!(!sql.contains("CROSS JOIN regions"));
    }
}
