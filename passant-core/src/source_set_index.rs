//! Inverted source-set index for multi-source policies.
//!
//! Multi-source policies are registered once under a canonical sorted source set.
//! Lookup walks only source sets reachable from visible query tables instead of
//! scanning the full multi-source bucket.

use std::collections::{HashMap, HashSet};

use smallvec::SmallVec;

use crate::identifiers::TableKey;
use crate::policy_index::PolicyIndex;
use crate::policy_store::{MultiSourceLookupMode, PolicyStore};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SourceSetLookupKey {
    sources: SmallVec<[TableKey; 4]>,
}

impl SourceSetLookupKey {
    fn from_sources(sources: &[TableKey]) -> Self {
        let mut sorted = sources.iter().cloned().collect::<SmallVec<[TableKey; 4]>>();
        sorted.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        Self { sources: sorted }
    }
}

/// Policies keyed by canonical multi-source sets with a per-table inverted index.
#[derive(Debug, Default, Clone)]
pub(crate) struct SourceSetPolicyIndex {
    sets: Vec<SmallVec<[TableKey; 4]>>,
    set_ids: HashMap<SourceSetLookupKey, usize>,
    policies_by_set: Vec<PolicyIndex>,
    set_ids_by_table: HashMap<TableKey, Vec<usize>>,
    all_policy_ids: Vec<usize>,
}

impl SourceSetPolicyIndex {
    pub(crate) fn register(&mut self, sources: &[TableKey], policy_id: usize) {
        debug_assert!(
            sources.len() > 1,
            "source-set index is for multi-source policies"
        );
        let key = SourceSetLookupKey::from_sources(sources);
        let set_id = if let Some(&existing) = self.set_ids.get(&key) {
            existing
        } else {
            let set_id = self.sets.len();
            self.sets.push(key.sources.clone());
            self.policies_by_set.push(PolicyIndex::default());
            self.set_ids.insert(key, set_id);
            for source in &self.sets[set_id] {
                self.set_ids_by_table
                    .entry(source.clone())
                    .or_default()
                    .push(set_id);
            }
            set_id
        };
        self.policies_by_set[set_id].push_id(policy_id);
        self.all_policy_ids.push(policy_id);
    }

    pub(crate) fn active_policy_ids<'a>(
        &'a self,
        store: &'a PolicyStore,
    ) -> impl Iterator<Item = usize> + 'a {
        self.all_policy_ids
            .iter()
            .copied()
            .filter(|&index| store.entries.get(index).is_some_and(|entry| entry.active))
    }

    /// Sorted policy-id lists for source sets visible under `mode` (no merge allocation).
    pub(crate) fn index_lists_for<'a>(
        &'a self,
        tables: &HashSet<TableKey>,
        mode: MultiSourceLookupMode,
    ) -> Vec<&'a PolicyIndex> {
        let set_ids = match mode {
            MultiSourceLookupMode::Subset => self.set_ids_for_subset(tables),
            MultiSourceLookupMode::AnyOverlap => self.set_ids_for_any_overlap(tables),
        };
        set_ids
            .into_iter()
            .map(|set_id| &self.policies_by_set[set_id])
            .collect()
    }

    pub(crate) fn estimated_bytes(&self) -> usize {
        let set_bytes = self
            .sets
            .iter()
            .map(|set| set.len() * std::mem::size_of::<TableKey>())
            .sum::<usize>();
        let inverted_bytes = self
            .set_ids_by_table
            .values()
            .map(|ids| ids.len() * std::mem::size_of::<usize>())
            .sum::<usize>();
        let policy_index_bytes = self
            .policies_by_set
            .iter()
            .map(index_memory_bytes)
            .sum::<usize>();
        set_bytes + inverted_bytes + policy_index_bytes
    }

    fn set_ids_for_any_overlap(&self, tables: &HashSet<TableKey>) -> Vec<usize> {
        let mut seen = HashSet::new();
        let mut set_ids = Vec::new();
        for table in tables {
            let Some(ids) = self.set_ids_by_table.get(table) else {
                continue;
            };
            for &set_id in ids {
                if seen.insert(set_id) {
                    set_ids.push(set_id);
                }
            }
        }
        set_ids
    }

    fn set_ids_for_subset(&self, tables: &HashSet<TableKey>) -> Vec<usize> {
        let mut hits = HashMap::<usize, usize>::new();
        for table in tables {
            let Some(ids) = self.set_ids_by_table.get(table) else {
                continue;
            };
            for &set_id in ids {
                *hits.entry(set_id).or_insert(0) += 1;
            }
        }
        hits.into_iter()
            .filter(|(set_id, count)| *count == self.sets[*set_id].len())
            .map(|(set_id, _)| set_id)
            .collect()
    }
}

fn index_memory_bytes(index: &PolicyIndex) -> usize {
    match index {
        PolicyIndex::List(list) => list.len() * std::mem::size_of::<usize>(),
        PolicyIndex::Bitmap(bitmap) => bitmap.serialized_size(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{PolicyIr, Resolution};

    fn dfc_multi(constraint: &str) -> PolicyIr {
        PolicyIr::Pgn {
            sources: vec!["foo".to_string(), "bar".to_string()],
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
    fn source_set_index_matches_subset_and_overlap_slow_scans() {
        let mut store = PolicyStore::default();
        store.register(dfc_multi("max(foo.id) + max(bar.id) > 1"));
        store.register(dfc_multi("max(foo.id) > 0"));

        let foo_only = HashSet::from([TableKey::new("foo")]);
        let both = HashSet::from([TableKey::new("foo"), TableKey::new("bar")]);

        store.assert_candidates_match_slow_scan(&foo_only, None, MultiSourceLookupMode::Subset);
        store.assert_candidates_match_slow_scan(&both, None, MultiSourceLookupMode::Subset);
        store.assert_candidates_match_slow_scan(&foo_only, None, MultiSourceLookupMode::AnyOverlap);
    }
}
