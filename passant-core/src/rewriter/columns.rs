use std::collections::{HashMap, HashSet};

use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Ident};

use crate::diagnostics::{ErrorKind, RewriteError};
use crate::identifiers::{AliasByBase, ColumnName, QualifiedColumn, TableName};

use super::helpers::OUTPUT_MARKER;

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
        Expr::IsNull(expr) => Expr::IsNull(Box::new(replace_source_alias_qualifiers(
            *expr,
            source_aliases,
        ))),
        Expr::IsNotNull(expr) => Expr::IsNotNull(Box::new(replace_source_alias_qualifiers(
            *expr,
            source_aliases,
        ))),
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
    ambiguous_output_columns: &HashSet<String>,
) -> Result<Expr, RewriteError> {
    let sink_overlaps_source = sources
        .iter()
        .any(|source| source.eq_ignore_ascii_case(sink));
    let mut expr = if !sink_expr_by_column.is_empty() || !ambiguous_output_columns.is_empty() {
        replace_sink_columns_strict(
            expr,
            OUTPUT_MARKER,
            sink_expr_by_column,
            ambiguous_output_columns,
        )?
    } else {
        expr
    };
    if let Some(sink_alias) = sink_alias {
        expr = replace_sink_columns(expr, sink_alias, sink_expr_by_column);
    }
    if !(sink_overlaps_source && sink_alias.is_some()) {
        expr = replace_sink_columns(expr, sink, sink_expr_by_column);
    }
    Ok(expr)
}

pub(crate) fn apply_output_marker_replacements(
    expr: Expr,
    output_expr_by_column: &HashMap<String, Expr>,
    ambiguous_output_columns: &HashSet<String>,
) -> Result<Expr, RewriteError> {
    if output_expr_by_column.is_empty() && ambiguous_output_columns.is_empty() {
        return Ok(expr);
    }
    replace_sink_columns_strict(
        expr,
        OUTPUT_MARKER,
        output_expr_by_column,
        ambiguous_output_columns,
    )
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
        Expr::IsNull(expr) => Expr::IsNull(Box::new(replace_sink_columns(
            *expr,
            sink,
            sink_expr_by_column,
        ))),
        Expr::IsNotNull(expr) => Expr::IsNotNull(Box::new(replace_sink_columns(
            *expr,
            sink,
            sink_expr_by_column,
        ))),
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

fn replace_sink_columns_strict(
    expr: Expr,
    sink: &str,
    sink_expr_by_column: &HashMap<String, Expr>,
    ambiguous_output_columns: &HashSet<String>,
) -> Result<Expr, RewriteError> {
    let sink_table = TableName::parse(sink);
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            if let Some(column) = QualifiedColumn::from_compound_identifier(&parts)
                && sink_table.matches_name(column.table.as_str())
            {
                let key = column.column.key();
                if ambiguous_output_columns.contains(&key) {
                    return Err(RewriteError::unsupported(
                        ErrorKind::InvalidSinkColumn,
                        format!(
                            "ambiguous _OUTPUT_ column '{}': multiple projected expressions share this output name",
                            column.column.as_str()
                        ),
                    ));
                }
                if let Some(replacement) = sink_expr_by_column.get(&key) {
                    return Ok(replacement.clone());
                }
                return Err(RewriteError::catalog(
                    ErrorKind::InvalidSinkColumn,
                    format!(
                        "{sink}.{} is not a projected output column",
                        column.column.as_str()
                    ),
                    Some(sink.to_string()),
                    Some(column.column.as_str().to_string()),
                ));
            }
            Ok(Expr::CompoundIdentifier(parts))
        }
        Expr::BinaryOp { left, op, right } => Ok(Expr::BinaryOp {
            left: Box::new(replace_sink_columns_strict(
                *left,
                sink,
                sink_expr_by_column,
                ambiguous_output_columns,
            )?),
            op,
            right: Box::new(replace_sink_columns_strict(
                *right,
                sink,
                sink_expr_by_column,
                ambiguous_output_columns,
            )?),
        }),
        Expr::Nested(expr) => Ok(Expr::Nested(Box::new(replace_sink_columns_strict(
            *expr,
            sink,
            sink_expr_by_column,
            ambiguous_output_columns,
        )?))),
        Expr::UnaryOp { op, expr } => Ok(Expr::UnaryOp {
            op,
            expr: Box::new(replace_sink_columns_strict(
                *expr,
                sink,
                sink_expr_by_column,
                ambiguous_output_columns,
            )?),
        }),
        Expr::IsNull(expr) => Ok(Expr::IsNull(Box::new(replace_sink_columns_strict(
            *expr,
            sink,
            sink_expr_by_column,
            ambiguous_output_columns,
        )?))),
        Expr::IsNotNull(expr) => Ok(Expr::IsNotNull(Box::new(replace_sink_columns_strict(
            *expr,
            sink,
            sink_expr_by_column,
            ambiguous_output_columns,
        )?))),
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => Ok(Expr::Case {
            operand: match operand {
                Some(expr) => Some(Box::new(replace_sink_columns_strict(
                    *expr,
                    sink,
                    sink_expr_by_column,
                    ambiguous_output_columns,
                )?)),
                None => None,
            },
            conditions: conditions
                .into_iter()
                .map(|expr| {
                    replace_sink_columns_strict(
                        expr,
                        sink,
                        sink_expr_by_column,
                        ambiguous_output_columns,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?,
            results: results
                .into_iter()
                .map(|expr| {
                    replace_sink_columns_strict(
                        expr,
                        sink,
                        sink_expr_by_column,
                        ambiguous_output_columns,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?,
            else_result: match else_result {
                Some(expr) => Some(Box::new(replace_sink_columns_strict(
                    *expr,
                    sink,
                    sink_expr_by_column,
                    ambiguous_output_columns,
                )?)),
                None => None,
            },
        }),
        Expr::Function(mut function) => {
            replace_sink_columns_strict_in_function(
                &mut function,
                sink,
                sink_expr_by_column,
                ambiguous_output_columns,
            )?;
            Ok(Expr::Function(function))
        }
        other => Ok(other),
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

fn replace_sink_columns_strict_in_function(
    function: &mut sqlparser::ast::Function,
    sink: &str,
    sink_expr_by_column: &HashMap<String, Expr>,
    ambiguous_output_columns: &HashSet<String>,
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
            } => {
                *expr = replace_sink_columns_strict(
                    expr.clone(),
                    sink,
                    sink_expr_by_column,
                    ambiguous_output_columns,
                )?;
            }
            _ => {}
        }
    }
    Ok(())
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

    #[test]
    fn source_alias_replacement_descends_through_nested_null_tests() {
        let aliases = HashMap::from([("src".to_string(), "receipts".to_string())]);
        for (input, expected) in [
            ("src.amount IS NULL", "receipts.amount IS NULL"),
            ("src.amount IS NOT NULL", "receipts.amount IS NOT NULL"),
            (
                "NOT ((src.amount IS NULL) IS NOT NULL)",
                "NOT ((receipts.amount IS NULL) IS NOT NULL)",
            ),
            (
                "CASE WHEN src.amount IS NULL THEN src.id IS NOT NULL ELSE false END",
                "CASE WHEN receipts.amount IS NULL THEN receipts.id IS NOT NULL ELSE false END",
            ),
            (
                "coalesce(src.amount IS NOT NULL, false)",
                "coalesce(receipts.amount IS NOT NULL, false)",
            ),
        ] {
            assert_eq!(
                replace_source_alias_qualifiers(parse_expr(input), &aliases),
                parse_expr(expected),
                "{input}"
            );
        }
    }

    #[test]
    fn sink_replacement_descends_through_nested_null_tests() {
        let columns = HashMap::from([("amount".to_string(), parse_expr("input.cost"))]);
        for (input, expected) in [
            ("reports.amount IS NULL", "input.cost IS NULL"),
            ("reports.amount IS NOT NULL", "input.cost IS NOT NULL"),
            (
                "NOT ((reports.amount IS NOT NULL) IS NULL)",
                "NOT ((input.cost IS NOT NULL) IS NULL)",
            ),
            (
                "coalesce(reports.amount IS NULL, false)",
                "coalesce(input.cost IS NULL, false)",
            ),
        ] {
            assert_eq!(
                replace_sink_columns(parse_expr(input), "reports", &columns),
                parse_expr(expected),
                "{input}"
            );
        }
    }

    #[test]
    fn output_marker_replacement_descends_through_nested_null_tests() {
        let columns = HashMap::from([("amount".to_string(), parse_expr("input.cost"))]);
        for (input, expected) in [
            ("_OUTPUT_.amount IS NULL", "input.cost IS NULL"),
            ("_OUTPUT_.amount IS NOT NULL", "input.cost IS NOT NULL"),
            (
                "NOT ((_OUTPUT_.amount IS NULL) IS NOT NULL)",
                "NOT ((input.cost IS NULL) IS NOT NULL)",
            ),
            (
                "CASE WHEN _OUTPUT_.amount IS NULL THEN false ELSE _OUTPUT_.amount IS NOT NULL END",
                "CASE WHEN input.cost IS NULL THEN false ELSE input.cost IS NOT NULL END",
            ),
        ] {
            assert_eq!(
                apply_output_marker_replacements(parse_expr(input), &columns, &HashSet::new())
                    .expect("valid output mapping"),
                parse_expr(expected),
                "{input}"
            );
        }
    }

    #[test]
    fn nested_null_output_references_still_reject_ambiguity_and_missing_columns() {
        let columns = HashMap::from([("amount".to_string(), parse_expr("input.cost"))]);
        let ambiguous = HashSet::from(["amount".to_string()]);
        for predicate in [
            "_OUTPUT_.amount IS NULL",
            "NOT ((_OUTPUT_.amount IS NOT NULL) IS NULL)",
        ] {
            let error =
                apply_output_marker_replacements(parse_expr(predicate), &columns, &ambiguous)
                    .expect_err("ambiguous output must fail closed");
            assert!(
                error.to_string().contains("ambiguous _OUTPUT_ column"),
                "{error}"
            );
        }
        for predicate in [
            "_OUTPUT_.missing IS NULL",
            "NOT ((_OUTPUT_.missing IS NOT NULL) IS NULL)",
        ] {
            let error =
                apply_output_marker_replacements(parse_expr(predicate), &columns, &HashSet::new())
                    .expect_err("missing output must fail closed");
            assert!(
                error.to_string().contains("not a projected output column"),
                "{error}"
            );
        }
    }

    #[test]
    fn null_replacements_preserve_unrelated_qualifiers() {
        let columns = HashMap::from([("amount".to_string(), parse_expr("input.cost"))]);
        let original = parse_expr("other.amount IS NULL OR other.amount IS NOT NULL");
        assert_eq!(
            replace_sink_columns(original.clone(), "reports", &columns),
            original
        );
        assert_eq!(
            apply_output_marker_replacements(original.clone(), &columns, &HashSet::new()).unwrap(),
            original
        );
    }
}
