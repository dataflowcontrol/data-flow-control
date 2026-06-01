use serde::{Deserialize, Serialize};
use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, FunctionArguments};

use crate::policy::PolicyIr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregateAnalysis {
    pub function_name: String,
    pub expression: String,
    pub distributive: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemiringAnalysis {
    pub aggregate_count: usize,
    pub all_distributive: bool,
    pub non_distributive_aggregates: Vec<String>,
}

impl Default for SemiringAnalysis {
    fn default() -> Self {
        Self {
            aggregate_count: 0,
            all_distributive: true,
            non_distributive_aggregates: Vec::new(),
        }
    }
}

pub fn analyze_policies(policies: &[PolicyIr]) -> SemiringAnalysis {
    let mut result = SemiringAnalysis::default();
    for policy in policies {
        let Ok(aggregates) = analyze_constraint(policy.constraint()) else {
            result.all_distributive = false;
            result
                .non_distributive_aggregates
                .push(format!("unparseable::{}", policy.constraint()));
            continue;
        };
        result.aggregate_count += aggregates.len();
        for aggregate in aggregates {
            if !aggregate.distributive {
                result.all_distributive = false;
                result
                    .non_distributive_aggregates
                    .push(aggregate.expression);
            }
        }
    }
    result
}

pub fn analyze_constraint(constraint: &str) -> Result<Vec<AggregateAnalysis>, String> {
    let expr = crate::sql::parse_policy_expr_duckdb(constraint).map_err(|err| err.to_string())?;
    Ok(analyze_constraint_expr(&expr))
}

pub fn analyze_constraint_expr(expr: &Expr) -> Vec<AggregateAnalysis> {
    let mut aggregates = Vec::new();
    collect_aggregates(expr, &mut aggregates);
    aggregates
}

pub fn semiring_analysis_from_expr(expr: &Expr) -> SemiringAnalysis {
    let aggregates = analyze_constraint_expr(expr);
    let mut non_distributive_aggregates = Vec::new();
    let mut all_distributive = true;
    for aggregate in &aggregates {
        if !aggregate.distributive {
            all_distributive = false;
            non_distributive_aggregates.push(aggregate.expression.clone());
        }
    }
    SemiringAnalysis {
        aggregate_count: aggregates.len(),
        all_distributive,
        non_distributive_aggregates,
    }
}

fn collect_aggregates(expr: &Expr, aggregates: &mut Vec<AggregateAnalysis>) {
    match expr {
        Expr::Function(function) => {
            let function_name = function.name.to_string();
            if is_known_aggregate(&function_name) {
                let distributive = is_semiring_distributive_aggregate(&function_name);
                aggregates.push(AggregateAnalysis {
                    function_name,
                    expression: crate::sql::render_expr(expr, None),
                    distributive,
                    reason: (!distributive).then_some(
                        "aggregate is not distributive in the supported semiring".into(),
                    ),
                });
            }
            collect_function_args(function, aggregates);
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_aggregates(left, aggregates);
            collect_aggregates(right, aggregates);
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => collect_aggregates(expr, aggregates),
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_aggregates(expr, aggregates);
            collect_aggregates(low, aggregates);
            collect_aggregates(high, aggregates);
        }
        Expr::InList { expr, list, .. } => {
            collect_aggregates(expr, aggregates);
            for item in list {
                collect_aggregates(item, aggregates);
            }
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                collect_aggregates(operand, aggregates);
            }
            for expr in conditions.iter().chain(results.iter()) {
                collect_aggregates(expr, aggregates);
            }
            if let Some(else_result) = else_result {
                collect_aggregates(else_result, aggregates);
            }
        }
        _ => {}
    }
}

fn collect_function_args(
    function: &sqlparser::ast::Function,
    aggregates: &mut Vec<AggregateAnalysis>,
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
            } => collect_aggregates(expr, aggregates),
            _ => {}
        }
    }
}

fn is_known_aggregate(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count"
            | "sum"
            | "min"
            | "max"
            | "bool_and"
            | "bool_or"
            | "avg"
            | "array_agg"
            | "string_agg"
            | "list"
    )
}

/// Native semiring aggregates (decompose across joins without a second pass).
fn is_native_distributive_aggregate(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count" | "sum" | "min" | "max" | "bool_and" | "bool_or"
    )
}

/// Aggregates treated as semiring-distributive via sum/count (or similar) decomposition.
fn is_decomposable_aggregate(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "avg")
}

/// Whether Full-Push may inline this aggregate using distributive semiring laws.
pub fn is_semiring_distributive_aggregate(name: &str) -> bool {
    is_native_distributive_aggregate(name) || is_decomposable_aggregate(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{PolicyIr, Resolution};

    #[test]
    fn classifies_distributive_and_non_distributive_aggregates() {
        let aggregates =
            analyze_constraint("sum(foo.amount) > avg(bar.amount) AND max(foo.id) > 1")
                .expect("constraint should analyze");
        assert!(
            aggregates
                .iter()
                .find(|a| a.function_name == "sum")
                .unwrap()
                .distributive
        );
        assert!(
            aggregates
                .iter()
                .find(|a| a.function_name == "avg")
                .unwrap()
                .distributive
        );
        assert!(
            aggregates
                .iter()
                .find(|a| a.function_name == "max")
                .unwrap()
                .distributive
        );
    }

    #[test]
    fn analyze_policies_marks_unparseable_constraints_non_distributive() {
        let policies = vec![PolicyIr::Pgn {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "max(foo.id) >".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }];
        let analysis = analyze_policies(&policies);
        assert!(!analysis.all_distributive);
        assert!(
            analysis
                .non_distributive_aggregates
                .iter()
                .any(|entry| entry.contains("unparseable"))
        );
    }

    #[test]
    fn string_agg_is_non_distributive() {
        let aggregates = analyze_constraint("string_agg(foo.name, ',') = 'x'")
            .expect("constraint should analyze");
        assert_eq!(aggregates.len(), 1);
        assert!(!aggregates[0].distributive);
    }
}
