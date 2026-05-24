use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Ident};

/// Rename `table.column` references to `alias.column`.
pub fn rename_table_refs(mut expr: Expr, table: &str, alias: &str) -> Expr {
    match &mut expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            if parts[0].value.eq_ignore_ascii_case(table) {
                parts[0] = Ident::new(alias);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            **left = rename_table_refs(*left.clone(), table, alias);
            **right = rename_table_refs(*right.clone(), table, alias);
        }
        Expr::Nested(inner) => {
            **inner = rename_table_refs(*inner.clone(), table, alias);
        }
        Expr::UnaryOp { expr: inner, .. } => {
            **inner = rename_table_refs(*inner.clone(), table, alias);
        }
        Expr::Function(function) => {
            if let FunctionArguments::List(args) = &mut function.args {
                for arg in &mut args.args {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(inner))
                        | FunctionArg::Named {
                            arg: FunctionArgExpr::Expr(inner),
                            ..
                        }
                        | FunctionArg::ExprNamed {
                            arg: FunctionArgExpr::Expr(inner),
                            ..
                        } => {
                            *inner = rename_table_refs(inner.clone(), table, alias);
                        }
                        _ => {}
                    }
                }
            }
            if let Some(filter) = function.filter.as_mut() {
                **filter = rename_table_refs(*filter.clone(), table, alias);
            }
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                **operand = rename_table_refs(*operand.clone(), table, alias);
            }
            for condition in conditions.iter_mut() {
                *condition = rename_table_refs(condition.clone(), table, alias);
            }
            for result in results.iter_mut() {
                *result = rename_table_refs(result.clone(), table, alias);
            }
            if let Some(else_result) = else_result {
                **else_result = rename_table_refs(*else_result.clone(), table, alias);
            }
        }
        Expr::IsFalse(inner)
        | Expr::IsNotFalse(inner)
        | Expr::IsTrue(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner) => {
            **inner = rename_table_refs(*inner.clone(), table, alias);
        }
        _ => {}
    }
    expr
}

/// Drop `table.` qualifiers from compound column references.
pub fn unqualify_table_refs(expr: Expr, table: &str) -> Expr {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            if parts[0].value.eq_ignore_ascii_case(table) {
                Expr::Identifier(parts.last().cloned().expect("compound column"))
            } else {
                Expr::CompoundIdentifier(parts)
            }
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(unqualify_table_refs(*left, table)),
            op,
            right: Box::new(unqualify_table_refs(*right, table)),
        },
        Expr::Nested(inner) => Expr::Nested(Box::new(unqualify_table_refs(*inner, table))),
        Expr::UnaryOp { op, expr } => Expr::UnaryOp {
            op,
            expr: Box::new(unqualify_table_refs(*expr, table)),
        },
        Expr::Function(mut function) => {
            if let FunctionArguments::List(args) = &mut function.args {
                for arg in &mut args.args {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(inner))
                        | FunctionArg::Named {
                            arg: FunctionArgExpr::Expr(inner),
                            ..
                        }
                        | FunctionArg::ExprNamed {
                            arg: FunctionArgExpr::Expr(inner),
                            ..
                        } => {
                            *inner = unqualify_table_refs(inner.clone(), table);
                        }
                        _ => {}
                    }
                }
            }
            if let Some(filter) = function.filter.as_mut() {
                **filter = unqualify_table_refs(*filter.clone(), table);
            }
            Expr::Function(function)
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => Expr::Case {
            operand: operand.map(|expr| Box::new(unqualify_table_refs(*expr, table))),
            conditions: conditions
                .into_iter()
                .map(|expr| unqualify_table_refs(expr, table))
                .collect(),
            results: results
                .into_iter()
                .map(|expr| unqualify_table_refs(expr, table))
                .collect(),
            else_result: else_result.map(|expr| Box::new(unqualify_table_refs(*expr, table))),
        },
        Expr::IsFalse(expr) => Expr::IsFalse(Box::new(unqualify_table_refs(*expr, table))),
        Expr::IsNotFalse(expr) => Expr::IsNotFalse(Box::new(unqualify_table_refs(*expr, table))),
        Expr::IsTrue(expr) => Expr::IsTrue(Box::new(unqualify_table_refs(*expr, table))),
        Expr::IsNotTrue(expr) => Expr::IsNotTrue(Box::new(unqualify_table_refs(*expr, table))),
        Expr::IsNull(expr) => Expr::IsNull(Box::new(unqualify_table_refs(*expr, table))),
        Expr::IsNotNull(expr) => Expr::IsNotNull(Box::new(unqualify_table_refs(*expr, table))),
        other => other,
    }
}

/// Replace exact expression subtrees with replacements (deepest match first).
pub fn replace_expr_subtrees(mut expr: Expr, replacements: &[(Expr, Expr)]) -> Expr {
    for (target, replacement) in replacements {
        expr = replace_expr_subtree(expr, target, replacement.clone());
    }
    expr
}

fn replace_expr_subtree(expr: Expr, target: &Expr, replacement: Expr) -> Expr {
    if expr == *target {
        return replacement;
    }
    match expr {
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(replace_expr_subtree(*left, target, replacement.clone())),
            op,
            right: Box::new(replace_expr_subtree(*right, target, replacement)),
        },
        Expr::Nested(inner) => {
            Expr::Nested(Box::new(replace_expr_subtree(*inner, target, replacement)))
        }
        Expr::UnaryOp { op, expr } => Expr::UnaryOp {
            op,
            expr: Box::new(replace_expr_subtree(*expr, target, replacement)),
        },
        Expr::Function(mut function) => {
            if let FunctionArguments::List(args) = &mut function.args {
                for arg in &mut args.args {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(inner))
                        | FunctionArg::Named {
                            arg: FunctionArgExpr::Expr(inner),
                            ..
                        }
                        | FunctionArg::ExprNamed {
                            arg: FunctionArgExpr::Expr(inner),
                            ..
                        } => {
                            *inner =
                                replace_expr_subtree(inner.clone(), target, replacement.clone());
                        }
                        _ => {}
                    }
                }
            }
            if let Some(filter) = function.filter.as_mut() {
                **filter = replace_expr_subtree(*filter.clone(), target, replacement.clone());
            }
            Expr::Function(function)
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => Expr::Case {
            operand: operand
                .map(|expr| Box::new(replace_expr_subtree(*expr, target, replacement.clone()))),
            conditions: conditions
                .into_iter()
                .map(|expr| replace_expr_subtree(expr, target, replacement.clone()))
                .collect(),
            results: results
                .into_iter()
                .map(|expr| replace_expr_subtree(expr, target, replacement.clone()))
                .collect(),
            else_result: else_result
                .map(|expr| Box::new(replace_expr_subtree(*expr, target, replacement.clone()))),
        },
        Expr::IsFalse(expr) => Expr::IsFalse(Box::new(replace_expr_subtree(
            *expr,
            target,
            replacement.clone(),
        ))),
        Expr::IsNotFalse(expr) => Expr::IsNotFalse(Box::new(replace_expr_subtree(
            *expr,
            target,
            replacement.clone(),
        ))),
        Expr::IsTrue(expr) => Expr::IsTrue(Box::new(replace_expr_subtree(
            *expr,
            target,
            replacement.clone(),
        ))),
        Expr::IsNotTrue(expr) => Expr::IsNotTrue(Box::new(replace_expr_subtree(
            *expr,
            target,
            replacement.clone(),
        ))),
        Expr::IsNull(expr) => Expr::IsNull(Box::new(replace_expr_subtree(
            *expr,
            target,
            replacement.clone(),
        ))),
        Expr::IsNotNull(expr) => Expr::IsNotNull(Box::new(replace_expr_subtree(
            *expr,
            target,
            replacement.clone(),
        ))),
        other => other,
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
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        match &select.projection[0] {
            sqlparser::ast::SelectItem::UnnamedExpr(expr) => expr.clone(),
            _ => panic!("expected unnamed expr"),
        }
    }

    #[test]
    fn rename_table_refs_does_not_touch_unrelated_columns() {
        let expr = parse_expr("max(foo.id_value) > 1");
        let renamed = rename_table_refs(expr, "foo", "f");
        assert_eq!(renamed.to_string(), "max(f.id_value) > 1");
    }

    #[test]
    fn rename_table_refs_preserves_other_tables() {
        let expr = parse_expr("max(foo.id) > max(bar.id)");
        let renamed = rename_table_refs(expr, "foo", "f");
        assert_eq!(renamed.to_string(), "max(f.id) > max(bar.id)");
    }
}
