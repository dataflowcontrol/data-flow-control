//! Qualified-column extraction from constraint ASTs.

use sqlparser::ast::{
    Array, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Map, Subscript,
};

use crate::identifiers::QualifiedColumn;

pub fn collect_qualified_columns_from_expr(expr: &Expr) -> Vec<QualifiedColumn> {
    let mut found = Vec::new();
    visit_expr(expr, &mut found);
    found
}

fn visit_expr(expr: &Expr, found: &mut Vec<QualifiedColumn>) {
    if let Some(column) = QualifiedColumn::from_expr(expr) {
        found.push(column);
    }
    match expr {
        Expr::UnaryOp { expr, .. } => visit_expr(expr, found),
        Expr::BinaryOp { left, right, .. } => {
            visit_expr(left, found);
            visit_expr(right, found);
        }
        Expr::Nested(inner) => visit_expr(inner, found),
        Expr::Function(function) => visit_function(function, found),
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                visit_expr(operand, found);
            }
            for (condition, result) in conditions.iter().zip(results.iter()) {
                visit_expr(condition, found);
                visit_expr(result, found);
            }
            if let Some(else_result) = else_result {
                visit_expr(else_result, found);
            }
        }
        Expr::InSubquery { expr, .. } | Expr::InList { expr, .. } => visit_expr(expr, found),
        Expr::Between {
            expr, low, high, ..
        } => {
            visit_expr(expr, found);
            visit_expr(low, found);
            visit_expr(high, found);
        }
        Expr::IsNull(expr) | Expr::IsNotNull(expr) => visit_expr(expr, found),
        Expr::Cast { expr, .. } => visit_expr(expr, found),
        Expr::Dictionary(fields) => {
            for field in fields {
                visit_expr(&field.value, found);
            }
        }
        Expr::Map(map) => visit_map(map, found),
        Expr::Array(array) => visit_array(array, found),
        Expr::Subscript { expr, subscript } => {
            visit_expr(expr, found);
            visit_subscript(subscript, found);
        }
        _ => {}
    }
}

fn visit_map(map: &Map, found: &mut Vec<QualifiedColumn>) {
    for entry in &map.entries {
        visit_expr(&entry.key, found);
        visit_expr(&entry.value, found);
    }
}

fn visit_array(array: &Array, found: &mut Vec<QualifiedColumn>) {
    for expr in &array.elem {
        visit_expr(expr, found);
    }
}

fn visit_subscript(subscript: &Subscript, found: &mut Vec<QualifiedColumn>) {
    match subscript {
        Subscript::Index { index } => visit_expr(index, found),
        Subscript::Slice {
            lower_bound,
            upper_bound,
            stride,
        } => {
            if let Some(lower_bound) = lower_bound {
                visit_expr(lower_bound, found);
            }
            if let Some(upper_bound) = upper_bound {
                visit_expr(upper_bound, found);
            }
            if let Some(stride) = stride {
                visit_expr(stride, found);
            }
        }
    }
}

fn visit_function(function: &Function, found: &mut Vec<QualifiedColumn>) {
    if let FunctionArguments::List(list) = &function.args {
        for arg in &list.args {
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
                visit_expr(expr, found);
            }
        }
    }
    if let Some(filter) = function.filter.as_ref() {
        visit_expr(filter, found);
    }
}
