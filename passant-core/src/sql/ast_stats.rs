//! AST node counting helpers for rewrite instrumentation.

use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Query, Select, SelectItem, SetExpr,
    Statement, TableFactor, TableWithJoins,
};

pub fn count_expr(expr: &Expr) -> usize {
    1 + match expr {
        Expr::BinaryOp { left, right, .. } => count_expr(left) + count_expr(right),
        Expr::Nested(inner)
        | Expr::UnaryOp { expr: inner, .. }
        | Expr::IsFalse(inner)
        | Expr::IsNotFalse(inner)
        | Expr::IsTrue(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner) => count_expr(inner),
        Expr::Subquery(query)
        | Expr::Exists {
            subquery: query, ..
        }
        | Expr::InSubquery {
            subquery: query, ..
        } => count_query(query),
        Expr::Function(function) => count_function(function),
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            operand.as_ref().map(|expr| count_expr(expr)).unwrap_or(0)
                + conditions.iter().map(count_expr).sum::<usize>()
                + results.iter().map(count_expr).sum::<usize>()
                + else_result
                    .as_ref()
                    .map(|expr| count_expr(expr))
                    .unwrap_or(0)
        }
        Expr::Between {
            expr, low, high, ..
        } => count_expr(expr) + count_expr(low) + count_expr(high),
        Expr::InList { expr, list, .. } => {
            count_expr(expr) + list.iter().map(count_expr).sum::<usize>()
        }
        Expr::Cast { expr, .. } => count_expr(expr),
        Expr::AtTimeZone { timestamp, .. } => count_expr(timestamp),
        _ => 0,
    }
}

fn count_function(function: &sqlparser::ast::Function) -> usize {
    let mut count = 1;
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
                } => count += count_expr(expr),
                _ => count += 1,
            }
        }
    }
    if let Some(filter) = function.filter.as_ref() {
        count += count_expr(filter);
    }
    count
}

pub fn count_select(select: &Select) -> usize {
    let mut count = 1;
    for table in &select.from {
        count += count_table_with_joins(table);
    }
    if let Some(expr) = select.selection.as_ref() {
        count += count_expr(expr);
    }
    if let Some(expr) = select.having.as_ref() {
        count += count_expr(expr);
    }
    for item in &select.projection {
        count += match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                count_expr(expr)
            }
            _ => 1,
        };
    }
    count
}

fn count_table_with_joins(table: &TableWithJoins) -> usize {
    let mut count = count_table_factor(&table.relation);
    for join in &table.joins {
        count += 1 + count_table_factor(&join.relation);
    }
    count
}

fn count_table_factor(factor: &TableFactor) -> usize {
    match factor {
        TableFactor::Derived { subquery, .. } => 1 + count_query(subquery),
        _ => 1,
    }
}

pub fn count_set_expr(set_expr: &SetExpr) -> usize {
    match set_expr {
        SetExpr::Select(select) => count_select(select),
        SetExpr::Query(query) => count_query(query),
        SetExpr::SetOperation { left, right, .. } => {
            1 + count_set_expr(left) + count_set_expr(right)
        }
        _ => 1,
    }
}

pub fn count_query(query: &Query) -> usize {
    let mut count = 1;
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            count += 1 + count_query(&cte.query);
        }
    }
    count += count_set_expr(query.body.as_ref());
    count
}

pub fn count_statement(statement: &Statement) -> usize {
    match statement {
        Statement::Query(query) => count_query(query),
        Statement::Insert(insert) => {
            1 + insert
                .source
                .as_ref()
                .map(|query| count_query(query))
                .unwrap_or(0)
        }
        Statement::Update {
            table,
            from,
            selection,
            ..
        } => {
            1 + count_table_with_joins(table)
                + from.as_ref().map(count_table_with_joins).unwrap_or(0)
                + selection.as_ref().map(count_expr).unwrap_or(0)
        }
        Statement::Merge {
            source: TableFactor::Derived { subquery, .. },
            ..
        } => 1 + count_query(subquery),
        _ => 1,
    }
}
