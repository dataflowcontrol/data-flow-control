use std::collections::HashSet;

use sqlparser::ast::{JoinOperator, Select, SetExpr, TableFactor, TableWithJoins};

use crate::identifiers::{TableKey, TableName};
use crate::policy::PolicyIr;
use crate::policy_store::PolicyStore;

pub fn set_expr_source_tables(set_expr: &SetExpr) -> HashSet<TableKey> {
    match set_expr {
        SetExpr::Select(select) => select_source_tables(select),
        SetExpr::Query(query) => set_expr_source_tables(query.body.as_ref()),
        SetExpr::SetOperation { left, right, .. } => {
            let mut tables = set_expr_source_tables(left);
            tables.extend(set_expr_source_tables(right));
            tables
        }
        _ => HashSet::new(),
    }
}

pub fn select_source_tables(select: &Select) -> HashSet<TableKey> {
    let mut tables = HashSet::new();
    for table in &select.from {
        tables.extend(table_with_joins_source_tables(table));
    }
    tables
}

pub fn select_nullable_source_tables(select: &Select) -> HashSet<TableKey> {
    let mut nullable = HashSet::new();
    for table in &select.from {
        let mut left_tables = table_factor_source_tables(&table.relation);
        for join in &table.joins {
            let right_tables = table_factor_source_tables(&join.relation);
            match join.join_operator {
                JoinOperator::LeftOuter(_) => nullable.extend(right_tables.iter().cloned()),
                JoinOperator::RightOuter(_) => nullable.extend(left_tables.iter().cloned()),
                JoinOperator::FullOuter(_) => {
                    nullable.extend(left_tables.iter().cloned());
                    nullable.extend(right_tables.iter().cloned());
                }
                _ => {}
            }
            left_tables.extend(right_tables);
        }
    }
    nullable
}

pub fn select_has_full_join(select: &Select) -> bool {
    select.from.iter().any(|table| {
        table
            .joins
            .iter()
            .any(|join| matches!(join.join_operator, JoinOperator::FullOuter(_)))
    })
}

pub fn select_has_anti_join(select: &Select) -> bool {
    select.from.iter().any(|table| {
        table.joins.iter().any(|join| {
            matches!(
                join.join_operator,
                JoinOperator::Anti(_) | JoinOperator::LeftAnti(_) | JoinOperator::RightAnti(_)
            )
        })
    })
}

pub fn set_operation_requires_cross_source_policies(
    policies: &[PolicyIr],
    left: &SetExpr,
    right: &SetExpr,
) -> bool {
    let left_tables = set_expr_source_tables(left);
    let right_tables = set_expr_source_tables(right);
    if left_tables.is_empty() || right_tables.is_empty() {
        return false;
    }
    let all_tables = left_tables
        .union(&right_tables)
        .cloned()
        .collect::<HashSet<_>>();

    policies.iter().any(|policy| {
        let sources = policy_source_keys(policy.sources());
        sources.len() > 1
            && sources.iter().all(|source| all_tables.contains(source))
            && (!sources.iter().all(|source| left_tables.contains(source))
                || !sources.iter().all(|source| right_tables.contains(source)))
    })
}

/// Indexed variant: only inspects multi-source policies registered in `PolicyStore`.
pub(crate) fn set_operation_requires_cross_source_policies_for_store(
    store: &PolicyStore,
    left: &SetExpr,
    right: &SetExpr,
) -> bool {
    let left_tables = set_expr_source_tables(left);
    let right_tables = set_expr_source_tables(right);
    if left_tables.is_empty() || right_tables.is_empty() {
        return false;
    }
    store
        .multi_source_policy_indices()
        .into_iter()
        .any(|index| {
            let Some(policy) = store.policy(index) else {
                return false;
            };
            policy_requires_set_split(policy, &left_tables, &right_tables)
        })
}

pub fn table_with_joins_source_tables(table: &TableWithJoins) -> HashSet<TableKey> {
    let mut tables = table_factor_source_tables(&table.relation);
    for join in &table.joins {
        tables.extend(table_factor_source_tables(&join.relation));
    }
    tables
}

pub fn table_factor_source_tables(factor: &TableFactor) -> HashSet<TableKey> {
    match factor {
        TableFactor::Table { name, .. } => {
            HashSet::from([TableKey::from_table(&TableName::from_object_name(name))])
        }
        TableFactor::Derived { subquery, .. } => set_expr_source_tables(subquery.body.as_ref()),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => table_with_joins_source_tables(table_with_joins),
        _ => HashSet::new(),
    }
}

pub fn policy_requires_set_split(
    policy: &PolicyIr,
    left_tables: &HashSet<TableKey>,
    right_tables: &HashSet<TableKey>,
) -> bool {
    let sources = policy_source_keys(policy.sources());
    sources.len() > 1
        && sources
            .iter()
            .all(|source| left_tables.contains(source) || right_tables.contains(source))
        && (!sources.iter().all(|source| left_tables.contains(source))
            || !sources.iter().all(|source| right_tables.contains(source)))
}

pub(crate) fn policy_source_keys(sources: &[String]) -> HashSet<TableKey> {
    sources.iter().map(|source| TableKey::new(source)).collect()
}
