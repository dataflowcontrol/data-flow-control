use sqlparser::ast::{BinaryOperator, DuplicateTreatment, Expr, FunctionArguments};

use crate::diagnostics::RewriteError;
use crate::policy_store::PolicyStore;
use crate::sql::{case_when, duckdb_array, int_literal, is_not_null, null_literal};

use super::expr::{first_function_expr, is_aggregate_name, parse_expr_or_identity};

pub(crate) fn transform_scan_aggregates(expr: Expr) -> Result<Expr, RewriteError> {
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
    if crate::sql::render_expr(&Expr::Value(value.clone()), None) != "1" {
        return Ok(None);
    }
    Ok(Some(is_not_null(col)))
}

fn count_distinct_inner_column(expr: &Expr) -> Option<Expr> {
    let Expr::Function(function) = expr else {
        return None;
    };
    if !function.name.to_string().eq_ignore_ascii_case("count") || !function_is_distinct(function) {
        return None;
    }
    first_function_expr(function)
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
            else_result: else_result
                .map(|expr| Box::new(transform_scan_aggregates_recursive(*expr))),
        },
        other => other,
    }
}

fn transform_scan_aggregate_function(function: sqlparser::ast::Function) -> Expr {
    let name = function.name.to_string();
    let lower = name.to_ascii_lowercase();
    if matches!(lower.as_str(), "count_if" | "countif") {
        if let Some(condition) = first_function_expr(&function) {
            return case_when(condition, int_literal(1), int_literal(0));
        }
        return int_literal(0);
    }
    if lower == "array_agg" {
        if let Some(column) = first_function_expr(&function) {
            return duckdb_array(column);
        }
        return duckdb_array(null_literal());
    }
    if is_count_like_aggregate(&lower, &function) {
        if function_is_distinct(&function)
            && let Some(column) = first_function_expr(&function)
        {
            return column;
        }
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

pub(crate) fn is_scan_transformable_non_distributive(aggregate: &str) -> bool {
    let lower = aggregate.to_ascii_lowercase();
    lower.contains("array_agg") || lower.contains("count_if") || lower.contains("countif")
}

pub(crate) fn finalize_policy_scan_ready(store: &mut PolicyStore, index: usize) {
    let Some(compiled) = store.compiled(index) else {
        return;
    };
    let Some(constraint) = compiled.constraint.as_ref() else {
        return;
    };
    if !compiled.semiring.all_distributive || compiled.semiring.aggregate_count == 0 {
        return;
    }
    if super::policy_expr::is_count_distinct_cardinality_one_check(&constraint.ast) {
        return;
    }
    if let Ok(scan_ready) = transform_scan_aggregates(constraint.ast.clone())
        && !super::expr::expr_contains_aggregate(&scan_ready)
    {
        store.set_scan_ready_expr(index, scan_ready);
    }
}

#[cfg(test)]
mod scan_ready_tests {
    use super::finalize_policy_scan_ready;
    use crate::policy::{PolicyIr, Resolution};
    use crate::policy_store::PolicyStore;

    fn remove_policy(source: &str, constraint: &str) -> PolicyIr {
        PolicyIr::Pgn {
            sources: vec![source.to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: constraint.to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }
    }

    #[test]
    fn finalize_policy_scan_ready_populates_scan_ready_expr() {
        let mut store = PolicyStore::default();
        let index = store.register(remove_policy("orders", "max(orders.amount) > 1"));

        finalize_policy_scan_ready(&mut store, index);

        assert!(store.scan_ready_expr(index).is_some());
    }
}
