use sqlparser::ast::{BinaryOperator, Expr, FunctionArguments, SelectItem, SetExpr, Statement};
use std::collections::HashMap;

use crate::parser::parse_query;
use crate::policy::{PolicyIr, Resolution};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ThresholdKey {
    pub lhs: String,
    pub direction: ThresholdDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ThresholdDirection {
    Greater,
    Less,
    Equal,
    NotEqual,
}

#[derive(Debug, Clone)]
pub(crate) struct ThresholdPredicate {
    pub key: ThresholdKey,
    pub value: f64,
    pub strict: bool,
}

pub fn threshold_dominates(left: &str, right: &str) -> bool {
    let Some(left) = threshold_predicate_from_policy_constraint(left) else {
        return false;
    };
    let Some(right) = threshold_predicate_from_policy_constraint(right) else {
        return false;
    };
    threshold_dominates_predicates(&left, &right)
}

pub(crate) fn threshold_predicate_from_policy(policy: &PolicyIr) -> Option<ThresholdPredicate> {
    let PolicyIr::Pgn {
        constraint,
        on_fail: Resolution::Remove,
        ..
    } = policy
    else {
        return None;
    };
    threshold_predicate_from_policy_constraint(constraint)
}

pub(crate) fn threshold_dominates_predicates(
    left: &ThresholdPredicate,
    right: &ThresholdPredicate,
) -> bool {
    if left.key != right.key {
        return false;
    }
    let same_value = (left.value - right.value).abs() < f64::EPSILON;
    match left.key.direction {
        ThresholdDirection::Greater => {
            left.value > right.value || (same_value && left.strict && !right.strict)
        }
        ThresholdDirection::Less => {
            left.value < right.value || (same_value && left.strict && !right.strict)
        }
        ThresholdDirection::Equal => left.value < right.value,
        ThresholdDirection::NotEqual => left.value < right.value,
    }
}

pub fn prune_dominated_remove_policies(policies: &[PolicyIr]) -> Vec<PolicyIr> {
    let applicable: Vec<(usize, ThresholdPredicate)> = policies
        .iter()
        .enumerate()
        .filter_map(|(index, policy)| {
            threshold_predicate_from_policy(policy).map(|predicate| (index, predicate))
        })
        .collect();

    let mut keep = vec![true; policies.len()];
    let mut strongest_by_key: HashMap<ThresholdKey, usize> = HashMap::new();

    for (index, candidate) in applicable {
        if let Some(existing_index) = strongest_by_key.get(&candidate.key).copied() {
            let Some(existing) = threshold_predicate_from_policy(&policies[existing_index]) else {
                continue;
            };
            if threshold_dominates_predicates(&existing, &candidate) {
                keep[index] = false;
                continue;
            }
            if threshold_dominates_predicates(&candidate, &existing) {
                keep[existing_index] = false;
                strongest_by_key.insert(candidate.key.clone(), index);
            }
        } else {
            strongest_by_key.insert(candidate.key.clone(), index);
        }
    }

    policies
        .iter()
        .enumerate()
        .filter(|(index, _)| keep[*index])
        .map(|(_, policy)| policy.clone())
        .collect()
}

fn threshold_predicate_from_policy_constraint(constraint: &str) -> Option<ThresholdPredicate> {
    let expr = parse_constraint_expr(constraint).ok()?;
    let Expr::BinaryOp { left, op, right } = expr else {
        return None;
    };
    let (direction, strict) = match op {
        BinaryOperator::Gt => (ThresholdDirection::Greater, true),
        BinaryOperator::GtEq => (ThresholdDirection::Greater, false),
        BinaryOperator::Lt => (ThresholdDirection::Less, true),
        BinaryOperator::LtEq => (ThresholdDirection::Less, false),
        BinaryOperator::Eq => (ThresholdDirection::Equal, true),
        BinaryOperator::NotEq => (ThresholdDirection::NotEqual, true),
        _ => return None,
    };
    let Expr::Value(value) = *right else {
        return None;
    };
    let value = value.to_string().parse::<f64>().ok()?;
    let lhs = if matches!(
        direction,
        ThresholdDirection::Greater | ThresholdDirection::Less
    ) {
        strip_supported_aggregates(*left).to_string()
    } else {
        left.to_string()
    };
    Some(ThresholdPredicate {
        key: ThresholdKey { lhs, direction },
        value,
        strict,
    })
}

fn parse_constraint_expr(sql: &str) -> Result<Expr, ()> {
    let statement = parse_query(&format!("SELECT {sql}")).map_err(|_| ())?;
    let Statement::Query(query) = statement else {
        return Err(());
    };
    let SetExpr::Select(select) = *query.body else {
        return Err(());
    };
    let item = select.projection.into_iter().next().ok_or(())?;
    match item {
        SelectItem::UnnamedExpr(expr) => Ok(expr),
        SelectItem::ExprWithAlias { expr, .. } => Ok(expr),
        _ => Err(()),
    }
}

fn strip_supported_aggregates(expr: Expr) -> Expr {
    match expr {
        Expr::Function(function) if is_aggregate_name(&function.name.to_string()) => {
            first_function_expr(&function).unwrap_or(Expr::Function(function))
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(strip_supported_aggregates(*left)),
            op,
            right: Box::new(strip_supported_aggregates(*right)),
        },
        Expr::Nested(expr) => Expr::Nested(Box::new(strip_supported_aggregates(*expr))),
        Expr::UnaryOp { op, expr } => Expr::UnaryOp {
            op,
            expr: Box::new(strip_supported_aggregates(*expr)),
        },
        other => other,
    }
}

fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count" | "sum" | "avg" | "min" | "max" | "array_agg" | "bool_and" | "bool_or"
    )
}

fn first_function_expr(function: &sqlparser::ast::Function) -> Option<Expr> {
    let FunctionArguments::List(args) = &function.args else {
        return None;
    };
    let first = args.args.first()?;
    match first {
        sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) => {
            Some(expr.clone())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::PolicyIr;

    fn remove_policy(constraint: &str) -> PolicyIr {
        PolicyIr::Pgn {
            sources: vec!["foo".to_string()],
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
    fn lower_bound_dominance_prefers_higher_threshold() {
        assert!(threshold_dominates("max(foo.id) > 10", "max(foo.id) > 5"));
        assert!(!threshold_dominates("max(foo.id) > 5", "max(foo.id) > 10"));
    }

    #[test]
    fn upper_bound_dominance_prefers_lower_threshold() {
        assert!(threshold_dominates("max(foo.id) < 5", "max(foo.id) < 10"));
        assert!(!threshold_dominates("max(foo.id) < 10", "max(foo.id) < 5"));
    }

    #[test]
    fn strict_threshold_dominates_non_strict_at_same_value() {
        assert!(threshold_dominates("max(foo.id) > 5", "max(foo.id) >= 5"));
        assert!(!threshold_dominates("max(foo.id) >= 5", "max(foo.id) > 5"));
    }

    #[test]
    fn prune_keeps_dominating_policy_only() {
        let policies = vec![
            remove_policy("max(foo.id) > 1"),
            remove_policy("max(foo.id) > 10"),
            remove_policy("max(bar.id) > 1"),
        ];
        let pruned = prune_dominated_remove_policies(&policies);
        assert_eq!(pruned.len(), 2);
        assert!(pruned.iter().any(|p| p.constraint() == "max(foo.id) > 10"));
        assert!(pruned.iter().any(|p| p.constraint() == "max(bar.id) > 1"));
    }

    #[test]
    fn non_remove_policies_are_not_pruned() {
        let policies = vec![
            PolicyIr::Pgn {
                sources: vec!["foo".to_string()],
                required_sources: Vec::new(),
                dimension_tables: Vec::new(),
                dimension_aliases: std::collections::HashMap::new(),
                dimension_queries: std::collections::HashMap::new(),
                sink: None,
                sink_alias: None,
                source_aliases: std::collections::HashMap::new(),
                constraint: "max(foo.id) > 1".to_string(),
                on_fail: Resolution::Kill,
                description: None,
            },
            remove_policy("max(foo.id) > 10"),
        ];
        let pruned = prune_dominated_remove_policies(&policies);
        assert_eq!(pruned.len(), 2);
    }
}
