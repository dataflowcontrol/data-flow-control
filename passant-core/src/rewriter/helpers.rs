use std::collections::{HashMap, HashSet};

use sqlparser::ast::{
    Assignment, AssignmentTarget, BinaryOperator, Expr, JoinOperator, Select, SetExpr, TableFactor,
    TableWithJoins,
};

use crate::identifiers::TableKey;
use crate::policy::PolicyIr;
use crate::policy_store::PolicyStore;
use crate::threshold;

use super::expr::{and_expr, projection_expr_and_name, table_factor_base_and_alias};
use super::types::PolicyApplicability;

pub(crate) fn flatten_and(expr: &Expr) -> Vec<Expr> {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            let mut conjuncts = flatten_and(left);
            conjuncts.extend(flatten_and(right));
            conjuncts
        }
        other => vec![other.clone()],
    }
}

pub(crate) fn rebuild_and(conjuncts: Vec<Expr>) -> Option<Expr> {
    if conjuncts.is_empty() {
        None
    } else {
        Some(
            conjuncts
                .into_iter()
                .reduce(and_expr)
                .expect("non-empty conjuncts"),
        )
    }
}

pub(crate) fn policy_description(policy: &PolicyIr) -> Option<&str> {
    match policy {
        PolicyIr::CompatDfc { description, .. } => description.as_deref(),
        PolicyIr::CompatAggregate(policy) => policy.description.as_deref(),
        PolicyIr::NativePgn(policy) => policy.description.as_deref(),
    }
}

pub(crate) fn direct_source_occurrence_counts(select: &Select) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for table in &select.from {
        if let Some((base, _)) = table_factor_base_and_alias(&table.relation) {
            *counts.entry(base.to_ascii_lowercase()).or_default() += 1;
        }
        for join in &table.joins {
            if let Some((base, _)) = table_factor_base_and_alias(&join.relation) {
                *counts.entry(base.to_ascii_lowercase()).or_default() += 1;
            }
        }
    }
    counts
}

pub(crate) fn table_joins_all_inner(table: &TableWithJoins) -> bool {
    !table.joins.is_empty()
        && table
            .joins
            .iter()
            .all(|join| matches!(join.join_operator, JoinOperator::Inner(_)))
}

pub(crate) fn table_with_joins_base_tables(table: &TableWithJoins) -> HashSet<TableKey> {
    let mut bases = HashSet::new();
    if let Some((base, _)) = table_factor_base_and_alias(&table.relation) {
        bases.insert(TableKey::new(&base));
    }
    for join in &table.joins {
        if let Some((base, _)) = table_factor_base_and_alias(&join.relation) {
            bases.insert(TableKey::new(&base));
        }
    }
    bases
}

pub(crate) fn prune_dominated_applicable_with_store<'a>(
    store: &PolicyStore,
    applicable: Vec<(usize, &'a PolicyIr, PolicyApplicability)>,
) -> (Vec<(usize, &'a PolicyIr, PolicyApplicability)>, usize) {
    let mut keep = vec![true; applicable.len()];
    let mut strongest_by_key: HashMap<
        threshold::ThresholdKey,
        (usize, threshold::ThresholdPredicate),
    > = HashMap::new();

    for (slot, (index, policy, applicability)) in applicable.iter().enumerate() {
        if *applicability != PolicyApplicability::Normal {
            continue;
        }
        let Some(candidate) = store
            .compiled(*index)
            .and_then(|entry| entry.threshold.clone())
            .or_else(|| threshold::threshold_predicate_from_policy(policy))
        else {
            continue;
        };
        if let Some((existing_slot, existing)) = strongest_by_key.get(&candidate.key).cloned() {
            if threshold::threshold_dominates_predicates(&existing, &candidate) {
                keep[slot] = false;
                continue;
            }
            if threshold::threshold_dominates_predicates(&candidate, &existing) {
                keep[existing_slot] = false;
                strongest_by_key.insert(candidate.key.clone(), (slot, candidate));
            }
        } else {
            strongest_by_key.insert(candidate.key.clone(), (slot, candidate));
        }
    }

    let dominated = keep.iter().filter(|&&k| !k).count();
    let kept = applicable
        .into_iter()
        .enumerate()
        .filter(|(slot, _)| keep[*slot])
        .map(|(_, item)| item)
        .collect();
    (kept, dominated)
}

#[allow(dead_code)]
pub(crate) fn prune_dominated_remove_policies(
    applicable: Vec<(&PolicyIr, PolicyApplicability)>,
) -> Vec<(&PolicyIr, PolicyApplicability)> {
    let indexed = applicable
        .into_iter()
        .enumerate()
        .map(|(index, (policy, applicability))| (index, policy, applicability))
        .collect::<Vec<_>>();
    let store = PolicyStore::default();
    prune_dominated_applicable_with_store(&store, indexed)
        .0
        .into_iter()
        .map(|(_, policy, applicability)| (policy, applicability))
        .collect()
}

pub(crate) fn insert_select_mapping(insert: &sqlparser::ast::Insert) -> HashMap<String, Expr> {
    let mut mapping = HashMap::new();
    let Some(query) = insert.source.as_ref() else {
        return mapping;
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return mapping;
    };

    for (index, item) in select.projection.iter().enumerate() {
        let Some((expr, alias)) = projection_expr_and_name(item) else {
            continue;
        };
        if let Some(column) = insert.columns.get(index) {
            mapping.insert(column.value.to_ascii_lowercase(), expr.clone());
        }
        if let Some(alias) = alias {
            mapping
                .entry(alias.to_ascii_lowercase())
                .or_insert(expr.clone());
        }
    }
    mapping
}

pub(crate) fn update_target_name(table: &TableWithJoins) -> Option<String> {
    match &table.relation {
        TableFactor::Table { name, .. } => Some(name.to_string()),
        _ => None,
    }
}

pub(crate) fn update_assignment_mapping(assignments: &[Assignment]) -> HashMap<String, Expr> {
    let mut mapping = HashMap::new();
    for assignment in assignments {
        let AssignmentTarget::ColumnName(name) = &assignment.target else {
            continue;
        };
        if let Some(column) = name.0.last() {
            mapping.insert(column.value.to_ascii_lowercase(), assignment.value.clone());
        }
    }
    mapping
}
