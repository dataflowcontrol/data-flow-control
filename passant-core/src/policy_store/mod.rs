use std::collections::HashMap;
use std::sync::Arc;

use crate::identifiers::{ColumnKey, TableKey, normalize_key};
use crate::intern::StringInterner;
use crate::policy::PolicyIr;
use crate::policy_compile::ParsedPolicyConstraint;
use crate::policy_index::PolicyIndex;
use crate::source_set_index::SourceSetPolicyIndex;

mod branch;
mod compiled;
mod delete;
mod indexes;
mod memory;

pub use branch::{BranchPolicyEntry, PolicyStoreView};
pub use compiled::{CompiledExpr, CompiledPolicy};
pub use memory::PolicyStoreMemoryUsage;

/// How multi-source policies are matched against visible query tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MultiSourceLookupMode {
    /// Include a multi-source policy only when all of its sources are visible.
    #[default]
    Subset,
    /// Include a multi-source policy when any source is visible (partial-push / branch-local).
    AnyOverlap,
}

/// Indexed registry of compiled policies for O(1) source/sink lookup.
#[derive(Debug, Default, Clone)]
pub struct PolicyStore {
    pub(crate) entries: Vec<Arc<CompiledPolicy>>,
    by_source: HashMap<TableKey, PolicyIndex>,
    by_sink: HashMap<TableKey, PolicyIndex>,
    join_pushdown_by_source: HashMap<TableKey, PolicyIndex>,
    enforcement_by_source: HashMap<TableKey, PolicyIndex>,
    /// Policies with no sources and no sink (true globals).
    global_no_source: PolicyIndex,
    enforcement_global_no_source: PolicyIndex,
    /// Multi-source enforcement policies (REMOVE/KILL) indexed by source set.
    enforcement_multi_source: SourceSetPolicyIndex,
    /// Multi-source policies indexed by canonical source set.
    multi_source: SourceSetPolicyIndex,
    policy_indices: Vec<usize>,
    remove_policy_count: usize,
    ui_policy_count: usize,
    tuple_resolution_policy_count: usize,
    relation_resolution_policy_count: usize,
    active_policy_count: usize,
    table_key_cache: HashMap<String, TableKey>,
    column_key_cache: HashMap<String, ColumnKey>,
    constraint_intern: StringInterner,
}

impl PolicyStore {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active_count() == 0
    }

    pub fn active_count(&self) -> usize {
        self.active_policy_count
    }

    pub fn register(&mut self, policy: PolicyIr) -> usize {
        self.register_maybe_parsed(policy, None)
    }

    pub(crate) fn register_with_parsed(
        &mut self,
        policy: PolicyIr,
        parsed: ParsedPolicyConstraint,
    ) -> usize {
        self.register_maybe_parsed(policy, Some(parsed))
    }

    fn register_maybe_parsed(
        &mut self,
        policy: PolicyIr,
        parsed: Option<ParsedPolicyConstraint>,
    ) -> usize {
        let index = self.entries.len();
        let compiled = Arc::new(self.compile_policy(index, policy, parsed));
        self.index_entry(&compiled);
        self.entries.push(compiled);
        self.active_policy_count += 1;
        index
    }

    /// Register multiple policies without re-building indexes between each insert.
    pub fn register_policies(
        &mut self,
        policies: impl IntoIterator<Item = PolicyIr>,
    ) -> Vec<usize> {
        policies
            .into_iter()
            .map(|policy| self.register(policy))
            .collect()
    }

    pub(crate) fn intern_table_key(&mut self, name: &str) -> TableKey {
        let normalized = normalize_key(name);
        if let Some(key) = self.table_key_cache.get(&normalized) {
            return key.clone();
        }
        let key = TableKey::from_arc(Arc::from(normalized.as_str()));
        self.table_key_cache.insert(normalized, key.clone());
        key
    }

    pub(crate) fn intern_column_key(&mut self, name: &str) -> ColumnKey {
        let normalized = normalize_key(name);
        if let Some(key) = self.column_key_cache.get(&normalized) {
            return key.clone();
        }
        let key = ColumnKey::from_arc(Arc::from(normalized.as_str()));
        self.column_key_cache.insert(normalized, key.clone());
        key
    }

    pub(crate) fn intern_string(&mut self, value: &str) -> Arc<str> {
        self.constraint_intern.intern(value)
    }

    pub fn deactivate(&mut self, index: usize) -> bool {
        let Some(entry) = self.entries.get(index) else {
            return false;
        };
        if !entry.active {
            return false;
        }
        if entry.policy.resolution() == crate::policy::Resolution::Remove {
            self.remove_policy_count = self.remove_policy_count.saturating_sub(1);
        }
        let resolution = entry.policy.resolution();
        let updated = Arc::new(CompiledPolicy {
            active: false,
            ..(**entry).clone()
        });
        self.adjust_resolution_counts(resolution, -1);
        self.entries[index] = updated;
        self.active_policy_count = self.active_policy_count.saturating_sub(1);
        true
    }

    pub fn policy(&self, index: usize) -> Option<&PolicyIr> {
        self.entries
            .get(index)
            .filter(|entry| entry.active)
            .map(|entry| &entry.policy)
    }

    pub fn compiled(&self, index: usize) -> Option<&CompiledPolicy> {
        self.entries
            .get(index)
            .filter(|entry| entry.active)
            .map(|entry| entry.as_ref())
    }

    pub fn entries(&self) -> &[Arc<CompiledPolicy>] {
        &self.entries
    }

    pub fn iter_active(&self) -> impl Iterator<Item = (usize, &PolicyIr)> + '_ {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if entry.active {
                    Some((index, &entry.policy))
                } else {
                    None
                }
            })
    }

    pub fn active_policies(&self) -> Vec<&PolicyIr> {
        self.iter_active().map(|(_, policy)| policy).collect()
    }

    pub fn policies_vec(&self) -> Vec<PolicyIr> {
        self.iter_active()
            .map(|(_, policy)| policy.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::policy::Resolution;
    use crate::sql::parse_projection_expr;

    fn pgn_policy(source: &str, constraint: &str) -> PolicyIr {
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
    fn indexed_candidates_match_slow_scan() {
        let mut store = PolicyStore::default();
        store.register(pgn_policy("orders", "max(orders.amount) > 1"));
        store.register(pgn_policy("customers", "max(customers.id) > 0"));
        store.register(pgn_policy("products", "max(products.id) > 0"));

        let mut tables = HashSet::new();
        tables.insert(TableKey::new("orders"));

        let indexed = store.candidate_ids_for_tables(&tables);
        store.assert_candidates_match_slow_scan(&tables, None, MultiSourceLookupMode::Subset);
        assert_eq!(indexed, vec![0]);
    }

    #[test]
    fn join_pushdown_index_only_contains_eligible_policies() {
        let mut store = PolicyStore::default();
        store.register(pgn_policy("foo", "max(foo.id) > 1"));
        store.register(PolicyIr::Pgn {
            sources: vec!["foo".to_string(), "bar".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "max(foo.id) + max(bar.id) > 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        });

        let foo = TableKey::new("foo");
        assert_eq!(store.join_pushdown_candidates(&foo), vec![0]);
    }

    #[test]
    fn sink_index_includes_sink_only_policies_for_scope_lookup() {
        let mut store = PolicyStore::default();
        store.register(PolicyIr::Pgn {
            sources: vec!["receipts".to_string()],
            required_sources: vec!["receipts".to_string()],
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: Some("reports".to_string()),
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "reports.id > 0".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        });

        let mut tables = HashSet::new();
        tables.insert(TableKey::new("other"));
        let sink = TableKey::new("reports");
        let indexed = store.candidate_ids_for_scope(&tables, Some(&sink));
        store.assert_candidates_match_slow_scan(
            &tables,
            Some(&sink),
            MultiSourceLookupMode::Subset,
        );
        assert_eq!(indexed, vec![0]);
    }

    #[test]
    fn resolution_counters_track_register_and_deactivate() {
        let mut store = PolicyStore::default();
        assert!(!store.has_ui_policies());
        assert!(!store.has_tuple_resolution_policies());
        assert!(!store.has_relation_resolution_policies());

        let ui_index = store.register(PolicyIr::Pgn {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "max(foo.id) > 0".to_string(),
            on_fail: Resolution::Ui,
            description: None,
        });
        assert!(store.has_ui_policies());

        let kill_index = store.register(PolicyIr::Pgn {
            sources: vec!["bar".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "bar.id > 0".to_string(),
            on_fail: Resolution::Kill,
            description: None,
        });
        assert!(store.has_tuple_resolution_policies());

        let rel_index = store.register(PolicyIr::Pgn {
            sources: vec![],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: Some("reports".to_string()),
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "max(reports.amount) <= 0".to_string(),
            on_fail: Resolution::RelationUdf("gate".to_string()),
            description: None,
        });
        assert!(store.has_relation_resolution_policies());

        assert!(store.deactivate(ui_index));
        assert!(!store.has_ui_policies());
        assert!(store.deactivate(kill_index));
        assert!(!store.has_tuple_resolution_policies());
        assert!(store.deactivate(rel_index));
        assert!(!store.has_relation_resolution_policies());
    }

    #[test]
    fn remove_policy_count_tracks_active_remove_policies() {
        let mut store = PolicyStore::default();
        assert!(!store.has_any_remove_policies());
        let remove_index = store.register(pgn_policy("foo", "foo.id > 1"));
        assert!(store.has_any_remove_policies());
        store.register(PolicyIr::Pgn {
            sources: vec!["bar".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "bar.id > 1".to_string(),
            on_fail: Resolution::Kill,
            description: None,
        });
        assert!(store.has_any_remove_policies());
        assert!(store.deactivate(remove_index));
        assert!(!store.has_any_remove_policies());
    }

    #[test]
    fn k_way_merge_matches_extend_sort_dedup() {
        let mut store = PolicyStore::default();
        for index in 0..6 {
            store.register(pgn_policy(&format!("source_{index}"), "id > 0"));
        }
        store.register(PolicyIr::Pgn {
            sources: vec!["shared".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "shared.id > 0".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        });
        store.register(PolicyIr::Pgn {
            sources: vec!["shared".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "shared.id > 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        });
        store.deactivate(2);

        let mut tables = HashSet::new();
        tables.insert(TableKey::new("source_0"));
        tables.insert(TableKey::new("source_3"));
        tables.insert(TableKey::new("shared"));

        let merged = store.candidate_ids_for_scope(&tables, None);
        let iter_merged: Vec<_> = store
            .candidate_scope_lookup(&tables, None, MultiSourceLookupMode::Subset)
            .iter()
            .collect();
        assert_eq!(merged, iter_merged);
        store.assert_candidates_match_slow_scan(&tables, None, MultiSourceLookupMode::Subset);

        let mut legacy = Vec::new();
        for table in &tables {
            if let Some(source_ids) = store.by_source.get(table) {
                legacy.extend(source_ids.collect_active_sorted(&store));
            }
        }
        legacy.extend(store.global_no_source.collect_active_sorted(&store));
        legacy.sort_unstable();
        legacy.dedup();
        assert_eq!(merged, legacy);
    }

    #[test]
    fn multi_source_enforcement_policy_caches_source_local_conjuncts() {
        let mut store = PolicyStore::default();
        let index = store.register(PolicyIr::Pgn {
            sources: vec!["foo".to_string(), "bar".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "max(foo.id) > 1 AND max(bar.id) > 10".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        });

        let compiled = store.compiled(index).expect("policy should be active");
        let conjuncts = compiled
            .source_local_conjuncts
            .as_ref()
            .expect("multi-source enforcement policy should cache conjuncts");
        assert_eq!(conjuncts.len(), 2);
        assert!(
            conjuncts
                .iter()
                .any(|(source, expr)| source.as_str() == "foo" && expr.to_string().contains("foo"))
        );
        assert!(
            conjuncts
                .iter()
                .any(|(source, expr)| source.as_str() == "bar" && expr.to_string().contains("bar"))
        );
        assert_eq!(
            compiled
                .constraint_referenced_sources
                .iter()
                .map(|key| key.as_str())
                .collect::<Vec<_>>(),
            vec!["bar", "foo"]
        );
    }

    #[test]
    fn active_policy_count_tracks_register_and_deactivate() {
        let mut store = PolicyStore::default();
        assert_eq!(store.active_count(), 0);
        let index = store.register(pgn_policy("foo", "foo.id > 1"));
        assert_eq!(store.active_count(), 1);
        assert!(store.deactivate(index));
        assert_eq!(store.active_count(), 0);
    }

    #[test]
    fn sink_only_policies_are_excluded_from_select_scope_without_sink() {
        let mut store = PolicyStore::default();
        store.register(pgn_policy("orders", "max(orders.amount) > 1"));
        store.register(PolicyIr::Pgn {
            sources: vec![],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: Some("reports".to_string()),
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "max(reports.amount) <= 0".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        });
        let mut tables = HashSet::new();
        tables.insert(TableKey::new("orders"));
        let candidates = store.candidate_ids_for_scope(&tables, None);
        assert_eq!(candidates, vec![0]);
        store.assert_candidates_match_slow_scan(&tables, None, MultiSourceLookupMode::Subset);
    }

    #[test]
    fn multi_source_policy_index_tracks_policies_with_multiple_sources() {
        let mut store = PolicyStore::default();
        store.register(pgn_policy("foo", "foo.id > 1"));
        store.register(PolicyIr::Pgn {
            sources: vec!["foo".to_string(), "bar".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "max(foo.id) + max(bar.id) > 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        });

        assert_eq!(store.multi_source_policy_indices(), vec![1]);
        let mut tables = HashSet::new();
        tables.insert(TableKey::new("foo"));
        assert_eq!(store.candidate_ids_for_scope(&tables, None), vec![0]);
        let foo_only = store
            .by_source
            .get(&TableKey::new("foo"))
            .expect("foo index");
        assert_eq!(foo_only.collect_active_sorted(&store), vec![0]);
    }

    #[test]
    fn multi_source_enforcement_policies_are_indexed_for_partial_push() {
        let mut store = PolicyStore::default();
        store.register(PolicyIr::Pgn {
            sources: vec!["foo".to_string(), "bar".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "avg(foo.id) > 1 AND avg(bar.id) > 10".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        });
        let mut tables = HashSet::new();
        tables.insert(TableKey::new("foo"));
        tables.insert(TableKey::new("bar"));
        assert_eq!(store.enforcement_candidate_ids_for_tables(&tables), vec![0]);
    }

    #[test]
    fn policy_indices_track_registered_pgn_policies() {
        let mut store = PolicyStore::default();
        store.register(pgn_policy("foo", "foo.id > 1"));
        store.register(pgn_policy("bar", "bar.id > 1"));

        assert_eq!(store.policy_indices(), vec![0, 1]);
    }

    #[test]
    fn delete_lookup_uses_source_and_sink_indexes() {
        let mut store = PolicyStore::default();
        store.register(PolicyIr::Pgn {
            sources: vec!["orders".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: Some("reports".to_string()),
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "reports.id > 0".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        });
        store.register(pgn_policy("other", "other.id > 1"));

        let sink = TableKey::new("reports");
        let ids = store.candidate_ids_for_delete_lookup(Some(&["orders".to_string()]), Some(&sink));
        assert_eq!(ids, vec![0]);
    }

    #[test]
    fn indexed_scope_lookup_matches_slow_scan_for_sink_and_sources() {
        let mut store = PolicyStore::default();
        store.register(pgn_policy("foo", "max(foo.id) > 1"));
        store.register(pgn_policy("bar", "max(bar.id) > 10"));
        store.register(pgn_policy("other", "max(other.id) > 0"));
        store.register(PolicyIr::Pgn {
            sources: vec!["receipts".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: Some("reports".to_string()),
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "reports.id > 0".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        });

        let mut tables = HashSet::new();
        tables.insert(TableKey::new("foo"));
        tables.insert(TableKey::new("bar"));
        store.assert_candidates_match_slow_scan(&tables, None, MultiSourceLookupMode::Subset);

        let sink = TableKey::new("reports");
        store.assert_candidates_match_slow_scan(
            &tables,
            Some(&sink),
            MultiSourceLookupMode::Subset,
        );
    }

    #[test]
    fn table_keys_are_interned_across_policies_on_same_source() {
        let mut store = PolicyStore::default();
        let first = store.register(pgn_policy("orders", "max(orders.amount) > 1"));
        let second = store.register(pgn_policy("orders", "max(orders.amount) > 2"));
        let first_key = store.compiled(first).expect("policy").source_keys[0].clone();
        let second_key = store.compiled(second).expect("policy").source_keys[0].clone();
        assert!(first_key.same_allocation_as(&second_key));
    }

    #[test]
    fn enforcement_index_only_tracks_remove_and_kill_policies() {
        let mut store = PolicyStore::default();
        store.register(pgn_policy("foo", "max(foo.id) > 1"));
        store.register(PolicyIr::Pgn {
            sources: vec![],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: Some("reports".to_string()),
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "max(reports.amount) <= 0".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        });
        store.register(PolicyIr::Pgn {
            sources: vec!["bar".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "max(bar.id) > 0".to_string(),
            on_fail: Resolution::Kill,
            description: None,
        });

        let foo = TableKey::new("foo");
        let bar = TableKey::new("bar");
        assert_eq!(
            store.enforcement_candidate_ids_for_tables(&HashSet::from([foo.clone()])),
            vec![0]
        );
        assert_eq!(
            store.enforcement_candidate_ids_for_tables(&HashSet::from([bar.clone()])),
            vec![2]
        );
    }

    #[test]
    fn dense_source_index_uses_bitmap_storage() {
        let mut store = PolicyStore::default();
        for index in 0..600usize {
            store.register(pgn_policy(
                "hot_source",
                &format!("max(hot_source.amount) > {index}"),
            ));
        }
        let hot = TableKey::new("hot_source");
        let source_index = store.by_source.get(&hot).expect("hot source index");
        assert!(source_index.is_bitmap());
        assert_eq!(source_index.len(), 600);
        assert_eq!(store.policies_for_source(&hot).len(), 600);
        let usage = store.memory_usage();
        assert!(usage.source_bitmap_indexes >= 1);
        assert!(usage.source_index_estimated_bytes < 600 * std::mem::size_of::<usize>());
    }

    #[test]
    fn identical_constraints_share_interned_sql() {
        let mut store = PolicyStore::default();
        let first = store.register(pgn_policy("orders", "max(orders.amount) > 1"));
        let second = store.register(pgn_policy("customers", "max(orders.amount) > 1"));
        let first_sql = store
            .compiled(first)
            .and_then(|entry| entry.constraint.as_ref())
            .map(|constraint| constraint.source_sql.clone());
        let second_sql = store
            .compiled(second)
            .and_then(|entry| entry.constraint.as_ref())
            .map(|constraint| constraint.source_sql.clone());
        assert!(Arc::ptr_eq(
            &first_sql.expect("first constraint"),
            &second_sql.expect("second constraint")
        ));
        assert_eq!(store.memory_usage().unique_constraint_strings, 1);
    }

    #[test]
    fn memory_usage_tracks_registry_growth() {
        let mut store = PolicyStore::default();
        for index in 0..128usize {
            store.register(pgn_policy(
                &format!("source_{index:03}"),
                "max(source.amount) > 1",
            ));
        }
        let usage = store.memory_usage();
        assert_eq!(usage.entry_count, 128);
        assert_eq!(usage.active_entries, 128);
        assert_eq!(usage.unique_constraint_strings, 1);
        assert_eq!(usage.unique_column_keys, 1);
        assert_eq!(usage.referenced_column_pairs, 128);
        assert!(usage.compiled_constraint_shared_bytes >= 128 * "max(source.amount) > 1".len());
    }

    #[test]
    fn branch_store_reuses_parent_table_key_interner() {
        let mut parent = PolicyStore::default();
        let parent_index = parent.register(pgn_policy("orders", "max(orders.amount) > 1"));
        let parent_key = parent.compiled(parent_index).expect("policy").source_keys[0].clone();

        let mut branch = PolicyStore::with_shared_interners(&parent);
        let branch_index = branch.register_branch_entries(vec![BranchPolicyEntry {
            policy: pgn_policy("orders", "max(orders.amount) > 2"),
            constraint_ast: parse_projection_expr("max(orders.amount) > 2").expect("parse"),
        }])[0];
        let branch_key = branch.compiled(branch_index).expect("policy").source_keys[0].clone();
        assert!(parent_key.same_allocation_as(&branch_key));
    }

    #[test]
    fn identical_columns_share_interned_keys() {
        let mut store = PolicyStore::default();
        let first = store.register(pgn_policy("orders", "max(orders.amount) > 1"));
        let second = store.register(pgn_policy("customers", "max(customers.amount) > 1"));
        let first_col = store
            .compiled(first)
            .expect("policy")
            .constraint_referenced_columns[0]
            .1
            .clone();
        let second_col = store
            .compiled(second)
            .expect("policy")
            .constraint_referenced_columns[0]
            .1
            .clone();
        assert!(first_col.same_allocation_as(&second_col));
        assert_eq!(store.memory_usage().unique_column_keys, 1);
    }
}
