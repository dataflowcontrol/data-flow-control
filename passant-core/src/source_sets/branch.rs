use sqlparser::ast::Expr;

use crate::policy::PolicyIr;
use crate::policy_store::{BranchPolicyEntry, PolicyStore};

use super::split::parse_constraint_expr;

pub(crate) fn branch_entry(
    store: &PolicyStore,
    parent_index: Option<usize>,
    policy: PolicyIr,
    constraint_ast: Option<Expr>,
) -> BranchPolicyEntry {
    let constraint_ast = constraint_ast
        .or_else(|| parent_index.and_then(|index| store.clone_constraint_ast(index)))
        .unwrap_or_else(|| {
            parse_constraint_expr(policy.constraint()).expect("branch policy constraint must parse")
        });
    BranchPolicyEntry {
        policy,
        constraint_ast,
    }
}
