use std::collections::HashSet;

use sqlparser::ast::{Expr, SetExpr};

use crate::identifiers::TableKey;
use crate::policy::{PolicyIr, Resolution};
use crate::policy_store::{BranchPolicyEntry, PolicyStore};

use super::branch::branch_entry;

use super::analysis::{policy_requires_set_split, policy_source_keys, set_expr_source_tables};
use super::split::{
    expr_referenced_policy_sources, join_conjuncts, parse_constraint_expr, split_conjuncts,
};

pub fn cross_source_policies_for_branch_indexed(
    store: &PolicyStore,
    branch_tables: &HashSet<TableKey>,
) -> Vec<BranchPolicyEntry> {
    store
        .multi_source_policy_indices()
        .into_iter()
        .filter_map(|index| {
            let policy = store.policy(index)?;
            if policy.sources().len() > 1
                && policy
                    .sources()
                    .iter()
                    .any(|source| branch_tables.contains(&TableKey::new(source)))
            {
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
            policy.sources().len() > 1
                && policy
                    .sources()
                    .iter()
                    .any(|source| branch_tables.contains(&TableKey::new(source)))
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
        left_policies.extend(branch_entries_from_split_policies(left_split));
        right_policies.extend(branch_entries_from_split_policies(right_split));
    }

    Some((left_policies, right_policies))
}

fn branch_entries_from_split_policies(policies: Vec<PolicyIr>) -> Vec<BranchPolicyEntry> {
    policies
        .into_iter()
        .map(|policy| {
            let ast = parse_constraint_expr(policy.constraint())
                .expect("split set-operation policy constraint must parse");
            BranchPolicyEntry {
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

pub fn split_policy_for_set_branches(
    policy: &PolicyIr,
    constraint_ast: Option<&Expr>,
    left_tables: &HashSet<TableKey>,
    right_tables: &HashSet<TableKey>,
) -> Option<(Vec<PolicyIr>, Vec<PolicyIr>)> {
    let PolicyIr::Pgn {
        sources,
        required_sources,
        dimension_tables,
        dimension_aliases,
        dimension_queries,
        sink,
        sink_alias,
        source_aliases,
        constraint,
        on_fail,
        description,
        ..
    } = policy;
    if sink.is_some() || !required_sources.is_empty() || !dimension_queries.is_empty() {
        return None;
    }
    let _ = dimension_tables;
    let _ = dimension_aliases;
    if !matches!(on_fail, Resolution::Remove | Resolution::Kill) {
        return None;
    }

    let policy_sources = policy_source_keys(sources);
    let expr = constraint_ast
        .cloned()
        .or_else(|| parse_constraint_expr(constraint).ok())?;
    let mut left_constraints = Vec::new();
    let mut right_constraints = Vec::new();
    for conjunct in split_conjuncts(expr) {
        let refs = expr_referenced_policy_sources(&conjunct, &policy_sources);
        if refs.is_empty() {
            left_constraints.push(conjunct.clone());
            right_constraints.push(conjunct);
            continue;
        }
        if refs.iter().all(|source| left_tables.contains(source)) {
            left_constraints.push(conjunct.clone());
        }
        if refs.iter().all(|source| right_tables.contains(source)) {
            right_constraints.push(conjunct);
        }
        if refs
            .iter()
            .any(|source| !left_tables.contains(source) && !right_tables.contains(source))
            || (!refs.iter().all(|source| left_tables.contains(source))
                && !refs.iter().all(|source| right_tables.contains(source)))
        {
            return None;
        }
    }

    let make_policy = |constraints: Vec<Expr>, tables: &HashSet<TableKey>| {
        if constraints.is_empty() {
            return None;
        }
        let branch_sources = sources
            .iter()
            .filter(|source| tables.contains(&TableKey::new(source)))
            .cloned()
            .collect::<Vec<_>>();
        if branch_sources.is_empty() {
            return None;
        }
        Some(PolicyIr::Pgn {
            sources: branch_sources.clone(),
            required_sources: Vec::new(),
            dimension_tables: dimension_tables.clone(),
            dimension_aliases: dimension_aliases.clone(),
            dimension_queries: dimension_queries.clone(),
            sink: None,
            sink_alias: sink_alias.clone(),
            source_aliases: source_aliases
                .iter()
                .filter(|(_, base)| branch_sources.iter().any(|s| s == *base))
                .map(|(alias, base)| (alias.clone(), base.clone()))
                .collect(),
            constraint: join_conjuncts(constraints).to_string(),
            on_fail: on_fail.clone(),
            description: description.clone(),
        })
    };

    Some((
        make_policy(left_constraints, left_tables)
            .into_iter()
            .collect(),
        make_policy(right_constraints, right_tables)
            .into_iter()
            .collect(),
    ))
}
