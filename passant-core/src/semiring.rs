use serde::{Deserialize, Serialize};
use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, FunctionArguments};

use crate::aggregate_registry::{AggregateRegistry, function_name};
use crate::policy::PolicyIr;
use crate::sql::SqlDialect;

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
    analyze_policies_with_registry(
        policies,
        &AggregateRegistry::for_dialect(SqlDialect::DuckDb),
    )
}

pub fn analyze_policies_with_registry(
    policies: &[PolicyIr],
    registry: &AggregateRegistry,
) -> SemiringAnalysis {
    let mut result = SemiringAnalysis::default();
    for policy in policies {
        let Ok(aggregates) = analyze_constraint_with_registry(policy.constraint(), registry) else {
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
    let registry = AggregateRegistry::for_dialect(SqlDialect::DuckDb);
    analyze_constraint_with_registry(constraint, &registry)
}

pub fn analyze_constraint_with_registry(
    constraint: &str,
    registry: &AggregateRegistry,
) -> Result<Vec<AggregateAnalysis>, String> {
    let expr = crate::sql::parse_policy_expr_duckdb(constraint).map_err(|err| err.to_string())?;
    Ok(analyze_constraint_expr_with_registry(&expr, registry))
}

pub fn analyze_constraint_expr(expr: &Expr) -> Vec<AggregateAnalysis> {
    let registry = AggregateRegistry::for_dialect(SqlDialect::DuckDb);
    analyze_constraint_expr_with_registry(expr, &registry)
}

pub fn analyze_constraint_expr_with_registry(
    expr: &Expr,
    registry: &AggregateRegistry,
) -> Vec<AggregateAnalysis> {
    let mut aggregates = Vec::new();
    collect_aggregates(expr, registry, &mut aggregates);
    aggregates
}

pub fn semiring_analysis_from_expr(expr: &Expr) -> SemiringAnalysis {
    let registry = AggregateRegistry::for_dialect(SqlDialect::DuckDb);
    semiring_analysis_from_expr_with_registry(expr, &registry)
}

pub fn semiring_analysis_from_expr_with_registry(
    expr: &Expr,
    registry: &AggregateRegistry,
) -> SemiringAnalysis {
    let aggregates = analyze_constraint_expr_with_registry(expr, registry);
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

fn collect_aggregates(
    expr: &Expr,
    registry: &AggregateRegistry,
    aggregates: &mut Vec<AggregateAnalysis>,
) {
    match expr {
        Expr::Function(function) => {
            let function_name = function_name(function);
            if registry.is_aggregate_name(&function_name) {
                let distributive = registry.is_semiring_distributive(&function_name);
                aggregates.push(AggregateAnalysis {
                    function_name: function_name.clone(),
                    expression: crate::sql::render_expr(expr, None),
                    distributive,
                    reason: (!distributive).then_some(
                        "aggregate is not distributive in the supported semiring".into(),
                    ),
                });
            }
            collect_function_args(function, registry, aggregates);
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_aggregates(left, registry, aggregates);
            collect_aggregates(right, registry, aggregates);
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => collect_aggregates(expr, registry, aggregates),
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_aggregates(expr, registry, aggregates);
            collect_aggregates(low, registry, aggregates);
            collect_aggregates(high, registry, aggregates);
        }
        Expr::InList { expr, list, .. } => {
            collect_aggregates(expr, registry, aggregates);
            for item in list {
                collect_aggregates(item, registry, aggregates);
            }
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                collect_aggregates(operand, registry, aggregates);
            }
            for expr in conditions.iter().chain(results.iter()) {
                collect_aggregates(expr, registry, aggregates);
            }
            if let Some(else_result) = else_result {
                collect_aggregates(else_result, registry, aggregates);
            }
        }
        _ => {}
    }
}

fn collect_function_args(
    function: &sqlparser::ast::Function,
    registry: &AggregateRegistry,
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
            } => collect_aggregates(expr, registry, aggregates),
            _ => {}
        }
    }
}

/// Whether Full-Push may inline this aggregate using distributive semiring laws.
pub fn is_semiring_distributive_aggregate(name: &str) -> bool {
    AggregateRegistry::for_dialect(SqlDialect::DuckDb).is_semiring_distributive(name)
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
                .find(|a| a.function_name.contains("sum"))
                .unwrap()
                .distributive
        );
        assert!(
            aggregates
                .iter()
                .find(|a| a.function_name.contains("avg"))
                .unwrap()
                .distributive
        );
        assert!(
            aggregates
                .iter()
                .find(|a| a.function_name.contains("max"))
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

    #[test]
    fn median_is_non_distributive() {
        let registry = AggregateRegistry::for_dialect(SqlDialect::DuckDb);
        let aggregates =
            analyze_constraint_with_registry("median(foo.amount) > 10", &registry).unwrap();
        assert_eq!(aggregates.len(), 1);
        assert!(!aggregates[0].distributive);
    }
}
