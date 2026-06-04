use sqlparser::ast::Expr;

use super::PolicyStore;
use super::compiled::compile_branch_policy;
use crate::policy::PolicyIr;

/// Branch-local policy plus a pre-parsed constraint AST (avoids re-parse at registration).
#[derive(Debug, Clone)]
pub struct BranchPolicyEntry {
    pub policy: PolicyIr,
    pub constraint_ast: Expr,
}

impl PolicyStore {
    /// Branch store sharing table/column/constraint interners with the parent registry.
    pub(crate) fn with_shared_interners(parent: &PolicyStore) -> Self {
        PolicyStore {
            table_key_cache: parent.table_key_cache.clone(),
            column_key_cache: parent.column_key_cache.clone(),
            constraint_intern: parent.constraint_intern.clone(),
            ..PolicyStore::default()
        }
    }

    pub(crate) fn register_branch_entries(
        &mut self,
        entries: impl IntoIterator<Item = BranchPolicyEntry>,
    ) -> Vec<usize> {
        entries
            .into_iter()
            .map(|entry| self.register_branch_entry(entry))
            .collect()
    }

    fn register_branch_entry(&mut self, entry: BranchPolicyEntry) -> usize {
        let index = self.entries.len();
        let compiled = compile_branch_policy(self, index, entry.policy, entry.constraint_ast);
        self.index_entry(&compiled);
        self.entries.push(std::sync::Arc::new(compiled));
        self.active_policy_count += 1;
        index
    }
}

/// Lightweight policy-store view for set-op / nullable-join branch rewrites.
///
/// Holds only branch-local policies while reusing the parent interner tables so split
/// policies do not rebuild the full registry or re-parse constraints from strings.
#[derive(Debug, Clone)]
pub struct PolicyStoreView {
    store: PolicyStore,
}

impl PolicyStoreView {
    pub fn build(parent: &PolicyStore, entries: Vec<BranchPolicyEntry>) -> Self {
        let mut store = PolicyStore::with_shared_interners(parent);
        store.register_branch_entries(entries);
        Self { store }
    }

    pub fn into_store(self) -> PolicyStore {
        self.store
    }

    pub fn store(&self) -> &PolicyStore {
        &self.store
    }
}
