use std::collections::{HashMap, HashSet};

use sqlparser::ast::{Query, Select, SetExpr, Statement};

use crate::identifiers::TableKey;
use crate::rewrite_stats::RewriteStatsCell;
use crate::source_sets::{
    select_has_anti_join, select_has_full_join, select_nullable_source_tables,
};
use crate::sql::ast_stats::count_statement;
use crate::statement_tables::{statement_sink_key, statement_table_keys};

use crate::rewriter::{TableScope, direct_source_occurrence_counts, select_is_aggregation};

/// Precomputed metadata for a single SELECT scope.
#[derive(Debug, Clone)]
pub struct SelectAnalysis {
    pub(crate) scope: TableScope,
    pub nullable_sources: HashSet<TableKey>,
    pub is_aggregation: bool,
    pub has_full_join: bool,
    pub has_anti_join: bool,
    pub source_occurrence_counts: HashMap<String, usize>,
}

impl SelectAnalysis {
    pub fn from_select(select: &Select) -> Self {
        Self {
            scope: TableScope::from_select(select),
            nullable_sources: select_nullable_source_tables(select),
            is_aggregation: select_is_aggregation(select),
            has_full_join: select_has_full_join(select),
            has_anti_join: select_has_anti_join(select),
            source_occurrence_counts: direct_source_occurrence_counts(select),
        }
    }

    pub(crate) fn from_table_scope(scope: TableScope) -> Self {
        Self {
            scope,
            nullable_sources: HashSet::new(),
            is_aggregation: false,
            has_full_join: false,
            has_anti_join: false,
            source_occurrence_counts: HashMap::new(),
        }
    }
}

/// Analysis of a parsed statement collected once before rewrite mutation.
#[derive(Debug, Clone, Default)]
pub struct StatementAnalysis {
    pub table_keys: HashSet<TableKey>,
    pub sink: Option<TableKey>,
    /// SELECT scopes in depth-first statement order.
    pub select_scopes: Vec<SelectAnalysis>,
}

impl StatementAnalysis {
    pub fn from_statement(statement: &Statement) -> Self {
        Self::from_statement_with_stats(statement, None)
    }

    pub(crate) fn from_statement_with_stats(
        statement: &Statement,
        stats: Option<&RewriteStatsCell>,
    ) -> Self {
        let mut analysis = Self {
            table_keys: statement_table_keys(statement),
            sink: statement_sink_key(statement),
            select_scopes: Vec::new(),
        };
        collect_select_scopes(statement, &mut analysis.select_scopes);
        if let Some(stats) = stats {
            stats.add_ast_nodes_visited_analysis(count_statement(statement));
        }
        analysis
    }
}

fn collect_select_scopes(statement: &Statement, scopes: &mut Vec<SelectAnalysis>) {
    match statement {
        Statement::Query(query) => collect_query_scopes(query, scopes),
        Statement::Insert(insert) => {
            if let Some(source) = &insert.source {
                collect_query_scopes(source, scopes);
            }
        }
        Statement::Update { table, from, .. } => {
            let mut scope = TableScope::default();
            scope.add_table_with_joins(table);
            if let Some(from) = from {
                scope.add_table_with_joins(from);
            }
            scopes.push(SelectAnalysis::from_table_scope(scope));
        }
        Statement::Merge {
            source: sqlparser::ast::TableFactor::Derived { subquery, .. },
            ..
        } => collect_query_scopes(subquery, scopes),
        _ => {}
    }
}

fn collect_query_scopes(query: &Query, scopes: &mut Vec<SelectAnalysis>) {
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            collect_query_scopes(&cte.query, scopes);
        }
    }
    collect_set_expr_scopes(query.body.as_ref(), scopes);
}

fn collect_set_expr_scopes(set_expr: &SetExpr, scopes: &mut Vec<SelectAnalysis>) {
    match set_expr {
        SetExpr::Select(select) => {
            scopes.push(SelectAnalysis::from_select(select));
            collect_select_nested_scopes(select, scopes);
        }
        SetExpr::Query(query) => collect_query_scopes(query, scopes),
        SetExpr::SetOperation { left, right, .. } => {
            collect_set_expr_scopes(left, scopes);
            collect_set_expr_scopes(right, scopes);
        }
        _ => {}
    }
}

fn collect_select_nested_scopes(select: &Select, scopes: &mut Vec<SelectAnalysis>) {
    for table in &select.from {
        collect_table_factor_scopes(&table.relation, scopes);
        for join in &table.joins {
            collect_table_factor_scopes(&join.relation, scopes);
        }
    }
    collect_expr_scopes(select.selection.as_ref(), scopes);
    collect_expr_scopes(select.having.as_ref(), scopes);
    for item in &select.projection {
        if let sqlparser::ast::SelectItem::UnnamedExpr(expr)
        | sqlparser::ast::SelectItem::ExprWithAlias { expr, .. } = item
        {
            collect_expr_scopes(Some(expr), scopes);
        }
    }
}

fn collect_table_factor_scopes(
    factor: &sqlparser::ast::TableFactor,
    scopes: &mut Vec<SelectAnalysis>,
) {
    if let sqlparser::ast::TableFactor::Derived { subquery, .. } = factor {
        collect_query_scopes(subquery, scopes);
    }
}

fn collect_expr_scopes(expr: Option<&sqlparser::ast::Expr>, scopes: &mut Vec<SelectAnalysis>) {
    let Some(expr) = expr else {
        return;
    };
    walk_expr_subqueries(expr, scopes);
}

fn walk_expr_subqueries(expr: &sqlparser::ast::Expr, scopes: &mut Vec<SelectAnalysis>) {
    match expr {
        sqlparser::ast::Expr::Subquery(query)
        | sqlparser::ast::Expr::Exists {
            subquery: query, ..
        }
        | sqlparser::ast::Expr::InSubquery {
            subquery: query, ..
        } => {
            collect_query_scopes(query, scopes);
        }
        sqlparser::ast::Expr::BinaryOp { left, right, .. } => {
            walk_expr_subqueries(left, scopes);
            walk_expr_subqueries(right, scopes);
        }
        sqlparser::ast::Expr::Nested(inner)
        | sqlparser::ast::Expr::UnaryOp { expr: inner, .. }
        | sqlparser::ast::Expr::IsFalse(inner)
        | sqlparser::ast::Expr::IsNotFalse(inner)
        | sqlparser::ast::Expr::IsTrue(inner)
        | sqlparser::ast::Expr::IsNotTrue(inner)
        | sqlparser::ast::Expr::IsNull(inner)
        | sqlparser::ast::Expr::IsNotNull(inner) => walk_expr_subqueries(inner, scopes),
        sqlparser::ast::Expr::Function(function) => {
            if let sqlparser::ast::FunctionArguments::List(args) = &function.args {
                for arg in &args.args {
                    if let sqlparser::ast::FunctionArg::Unnamed(
                        sqlparser::ast::FunctionArgExpr::Expr(inner),
                    )
                    | sqlparser::ast::FunctionArg::Named {
                        arg: sqlparser::ast::FunctionArgExpr::Expr(inner),
                        ..
                    }
                    | sqlparser::ast::FunctionArg::ExprNamed {
                        arg: sqlparser::ast::FunctionArgExpr::Expr(inner),
                        ..
                    } = arg
                    {
                        walk_expr_subqueries(inner, scopes);
                    }
                }
            }
            if let Some(filter) = function.filter.as_ref() {
                walk_expr_subqueries(filter, scopes);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_query;

    #[test]
    fn statement_analysis_collects_cte_and_main_select_scopes() {
        let statement = parse_query(
            "WITH cte AS (SELECT id FROM foo) SELECT bar.id FROM bar JOIN cte ON bar.id = cte.id",
        )
        .expect("parse");
        let analysis = StatementAnalysis::from_statement(&statement);
        assert!(analysis.table_keys.contains(&TableKey::new("foo")));
        assert!(analysis.table_keys.contains(&TableKey::new("bar")));
        assert_eq!(analysis.select_scopes.len(), 2);
    }

    #[test]
    fn statement_analysis_records_ast_visit_counts() {
        let statement = parse_query(
            "WITH cte AS (SELECT id FROM foo) SELECT bar.id FROM bar JOIN cte ON bar.id = cte.id",
        )
        .expect("parse");
        let stats = RewriteStatsCell::default();
        StatementAnalysis::from_statement_with_stats(&statement, Some(&stats));
        assert!(stats.snapshot().ast_nodes_visited_analysis > 0);
    }
}
