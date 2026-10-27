use std::collections::HashSet;

use sqlparser::ast::{Expr, SetExpr};

use crate::identifiers::TableKey;
use crate::policy::PolicyIr;
use crate::policy_store::{BranchPolicyEntry, PolicyStore};

use super::branch::branch_entry;

use super::analysis::{policy_requires_set_split, set_expr_source_tables};
use super::split::parse_constraint_expr;

pub fn cross_source_policies_for_branch_indexed(
    store: &PolicyStore,
    branch_tables: &HashSet<TableKey>,
) -> Vec<BranchPolicyEntry> {
    store
        .multi_source_policy_indices()
        .into_iter()
        .filter_map(|index| {
            let policy = store.policy(index)?;
            if policy.sources().len() > 1 && branch_has_policy_scope(policy, branch_tables) {
                Some(branch_entry(store, Some(index), policy.clone(), None))
            } else {
                None
            }
        })
        .collect()
}

pub fn cross_source_policies_for_branch(
    policies: &[PolicyIr],
    branch_tables: &HashSet<TableKey>,
) -> Vec<PolicyIr> {
    policies
        .iter()
        .filter(|policy| {
            policy.sources().len() > 1 && branch_has_policy_scope(policy, branch_tables)
        })
        .cloned()
        .collect()
}

pub fn split_set_operation_policies_for_store(
    store: &PolicyStore,
    left: &SetExpr,
    right: &SetExpr,
) -> Option<(Vec<BranchPolicyEntry>, Vec<BranchPolicyEntry>)> {
    let left_tables = set_expr_source_tables(left);
    let right_tables = set_expr_source_tables(right);
    let mut all_tables = left_tables.clone();
    all_tables.extend(right_tables.iter().cloned());
    let scope_policies = store.candidate_entries_for_scope(&all_tables, None);
    split_set_operation_policy_entries(&scope_policies, store, left, right)
}

pub fn split_set_operation_policies(
    policies: &[PolicyIr],
    left: &SetExpr,
    right: &SetExpr,
) -> Option<(Vec<PolicyIr>, Vec<PolicyIr>)> {
    let left_tables = set_expr_source_tables(left);
    let right_tables = set_expr_source_tables(right);
    let mut left_policies = Vec::new();
    let mut right_policies = Vec::new();

    for policy in policies {
        if !policy_requires_set_split(policy, &left_tables, &right_tables) {
            left_policies.push(policy.clone());
            right_policies.push(policy.clone());
            continue;
        }
        let (left_split, right_split) =
            split_policy_for_set_branches(policy, None, &left_tables, &right_tables)?;
        left_policies.extend(left_split);
        right_policies.extend(right_split);
    }

    Some((left_policies, right_policies))
}

fn split_set_operation_policy_entries(
    policies: &[(usize, PolicyIr)],
    store: &PolicyStore,
    left: &SetExpr,
    right: &SetExpr,
) -> Option<(Vec<BranchPolicyEntry>, Vec<BranchPolicyEntry>)> {
    let left_tables = set_expr_source_tables(left);
    let right_tables = set_expr_source_tables(right);
    let mut left_policies = Vec::new();
    let mut right_policies = Vec::new();

    for (index, policy) in policies {
        let compiled = store.compiled(*index);
        if !multi_source_policy_needs_set_split(policy, compiled, &left_tables, &right_tables) {
            let entry = branch_entry(store, Some(*index), policy.clone(), None);
            left_policies.push(entry.clone());
            right_policies.push(entry);
            continue;
        }
        let constraint_ast = store.clone_constraint_ast(*index);
        let (left_split, right_split) = split_policy_for_set_branches(
            policy,
            constraint_ast.as_ref(),
            &left_tables,
            &right_tables,
        )?;
        left_policies.extend(branch_entries_from_split_policies(
            left_split,
            store.original_policy_index(*index),
        ));
        right_policies.extend(branch_entries_from_split_policies(
            right_split,
            store.original_policy_index(*index),
        ));
    }

    Some((left_policies, right_policies))
}

fn branch_entries_from_split_policies(
    policies: Vec<PolicyIr>,
    original_policy_index: Option<usize>,
) -> Vec<BranchPolicyEntry> {
    policies
        .into_iter()
        .map(|policy| {
            let ast = parse_constraint_expr(policy.constraint())
                .expect("split set-operation policy constraint must parse");
            BranchPolicyEntry {
                original_policy_index,
                policy,
                constraint_ast: ast,
            }
        })
        .collect()
}

fn multi_source_policy_needs_set_split(
    policy: &PolicyIr,
    compiled: Option<&crate::policy_store::CompiledPolicy>,
    left_tables: &HashSet<TableKey>,
    right_tables: &HashSet<TableKey>,
) -> bool {
    if !policy_requires_set_split(policy, left_tables, right_tables) {
        return false;
    }
    let Some(compiled) = compiled else {
        return true;
    };
    if compiled.constraint_referenced_sources.is_empty() {
        return true;
    }
    compiled
        .constraint_referenced_sources
        .iter()
        .any(|source| left_tables.contains(source))
        && compiled
            .constraint_referenced_sources
            .iter()
            .any(|source| right_tables.contains(source))
}

/// Route whole ordinary policies; splitting a predicate must never narrow SOURCE scope.
/// This branch-local helper does not merge provenance across duplicate-eliminating
/// set operations; their caller must use a provenance-aware joint finalizer.
pub fn split_policy_for_set_branches(
    policy: &PolicyIr,
    _constraint_ast: Option<&Expr>,
    left_tables: &HashSet<TableKey>,
    right_tables: &HashSet<TableKey>,
) -> Option<(Vec<PolicyIr>, Vec<PolicyIr>)> {
    // Retain the established REQUIRED routing contract, not ordinary skip semantics.
    if !policy.required_sources().is_empty() {
        return None;
    }
    let branch_policy = |tables: &HashSet<TableKey>| {
        policy
            .sources()
            .iter()
            .all(|source| tables.contains(&TableKey::new(source)))
            .then(|| policy.clone())
            .into_iter()
            .collect()
    };
    Some((branch_policy(left_tables), branch_policy(right_tables)))
}

fn branch_has_policy_scope(policy: &PolicyIr, tables: &HashSet<TableKey>) -> bool {
    if policy.required_sources().is_empty() {
        policy
            .sources()
            .iter()
            .all(|source| tables.contains(&TableKey::new(source)))
    } else {
        policy
            .sources()
            .iter()
            .any(|source| tables.contains(&TableKey::new(source)))
    }
}
