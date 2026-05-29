use std::collections::HashMap;

use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Ident};

use crate::identifiers::{AliasByBase, ColumnName, QualifiedColumn, TableName};

pub(crate) fn replace_source_alias_qualifiers(
    expr: Expr,
    source_aliases: &HashMap<String, String>,
) -> Expr {
    if source_aliases.is_empty() {
        return expr;
    }
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            if let Some(column) = QualifiedColumn::from_compound_identifier(&parts) {
                let alias_key = column.table.as_str().to_ascii_lowercase();
                if let Some(base) = source_aliases.get(&alias_key) {
                    let mut new_parts = vec![Ident::new(base)];
                    new_parts.extend_from_slice(&parts[1..]);
                    return Expr::CompoundIdentifier(new_parts);
                }
            }
            Expr::CompoundIdentifier(parts)
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(replace_source_alias_qualifiers(*left, source_aliases)),
            op,
            right: Box::new(replace_source_alias_qualifiers(*right, source_aliases)),
        },
        Expr::Nested(expr) => Expr::Nested(Box::new(replace_source_alias_qualifiers(
            *expr,
            source_aliases,
        ))),
        Expr::UnaryOp { op, expr } => Expr::UnaryOp {
            op,
            expr: Box::new(replace_source_alias_qualifiers(*expr, source_aliases)),
        },
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => Expr::Case {
            operand: operand
                .map(|expr| Box::new(replace_source_alias_qualifiers(*expr, source_aliases))),
            conditions: conditions
                .into_iter()
                .map(|expr| replace_source_alias_qualifiers(expr, source_aliases))
                .collect(),
            results: results
                .into_iter()
                .map(|expr| replace_source_alias_qualifiers(expr, source_aliases))
                .collect(),
            else_result: else_result
                .map(|expr| Box::new(replace_source_alias_qualifiers(*expr, source_aliases))),
        },
        Expr::Function(mut function) => {
            replace_source_alias_qualifiers_in_function(&mut function, source_aliases);
            Expr::Function(function)
        }
        other => other,
    }
}

pub(crate) fn apply_policy_sink_column_replacements(
    expr: Expr,
    sink: &str,
    sink_alias: &Option<String>,
    sources: &[String],
    sink_expr_by_column: &HashMap<String, Expr>,
) -> Expr {
    let sink_overlaps_source = sources
        .iter()
        .any(|source| source.eq_ignore_ascii_case(sink));
    let mut expr = replace_sink_columns(expr, "_OUTPUT_", sink_expr_by_column);
    if let Some(sink_alias) = sink_alias {
        expr = replace_sink_columns(expr, sink_alias, sink_expr_by_column);
    }
    if !(sink_overlaps_source && sink_alias.is_some()) {
        expr = replace_sink_columns(expr, sink, sink_expr_by_column);
    }
    expr
}

pub(crate) fn replace_sink_columns(
    expr: Expr,
    sink: &str,
    sink_expr_by_column: &HashMap<String, Expr>,
) -> Expr {
    let sink_table = TableName::parse(sink);
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            if let Some(column) = QualifiedColumn::from_compound_identifier(&parts)
                && sink_table.matches_name(column.table.as_str())
            {
                return sink_expr_by_column
                    .get(&column.column.key())
                    .cloned()
                    .unwrap_or(Expr::CompoundIdentifier(parts));
            }
            Expr::CompoundIdentifier(parts)
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

pub(crate) fn collect_compound_columns_by_name(expr: &Expr, columns: &mut HashMap<String, Expr>) {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            if let Some(column) = parts.last() {
                columns
                    .entry(ColumnName::from_ident(column).key())
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

pub(crate) fn replace_identifiers(expr: &mut Expr, replacements: &HashMap<String, String>) {
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

fn replace_source_alias_qualifiers_in_function(
    function: &mut sqlparser::ast::Function,
    source_aliases: &HashMap<String, String>,
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
                *expr = replace_source_alias_qualifiers(expr.clone(), source_aliases);
            }
            _ => {}
        }
    }
    if let Some(filter) = &mut function.filter {
        **filter = replace_source_alias_qualifiers((**filter).clone(), source_aliases);
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

pub(crate) fn rewrite_column_qualifiers(expr: &mut Expr, alias_by_base: &AliasByBase) {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            if let Some(column) = QualifiedColumn::from_compound_identifier(parts)
                && let Some(alias) = alias_by_base.get(&column.table)
            {
                let column_ident = parts.last().cloned().expect("qualified column");
                *parts = vec![Ident::new(alias), column_ident];
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

pub(crate) fn unqualify_columns(expr: &mut Expr) {
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

fn rewrite_function_args(function: &mut sqlparser::ast::Function, alias_by_base: &AliasByBase) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::DuckDbDialect;
    use sqlparser::parser::Parser;

    fn parse_expr(sql: &str) -> Expr {
        let mut statements =
            Parser::parse_sql(&DuckDbDialect {}, &format!("SELECT {sql}")).expect("parse");
        let sqlparser::ast::Statement::Query(query) = statements.remove(0) else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(mut select) = *query.body else {
            panic!("expected select");
        };
        match select.projection.remove(0) {
            sqlparser::ast::SelectItem::UnnamedExpr(expr) => expr,
            _ => panic!("expected unnamed expr"),
        }
    }

    #[test]
    fn rewrite_column_qualifiers_replaces_schema_qualified_table_prefix() {
        let mut expr = parse_expr("\"MySchema\".\"MyTable\".\"OrderID\" > 1");
        rewrite_column_qualifiers(
            &mut expr,
            &AliasByBase::single("\"MySchema\".\"MyTable\"", "mt"),
        );
        assert_eq!(expr.to_string(), "mt.\"OrderID\" > 1");
    }
}
