use std::collections::HashSet;

use super::PolicyStore;
use crate::identifiers::TableKey;

impl PolicyStore {
    /// Narrow delete lookups using source/sink indexes instead of scanning all policies.
    pub fn candidate_ids_for_delete_lookup(
        &self,
        sources: Option<&[String]>,
        sink: Option<&TableKey>,
    ) -> Vec<usize> {
        let ids = match (sources, sink) {
            (Some(source_list), Some(sink_key)) => {
                let mut source_ids = HashSet::new();
                for source in source_list {
                    let key = TableKey::new(source);
                    if let Some(indexes) = self.by_source.get(&key) {
                        source_ids.extend(indexes.collect_active_sorted(self));
                    }
                }
                let sink_ids: HashSet<usize> = self
                    .by_sink
                    .get(sink_key)
                    .map(|indexes| indexes.collect_active_sorted(self))
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                source_ids
                    .into_iter()
                    .filter(|index| sink_ids.contains(index))
                    .collect::<HashSet<_>>()
            }
            (Some(source_list), None) => {
                let mut source_ids = HashSet::new();
                for source in source_list {
                    if let Some(indexes) = self.by_source.get(&TableKey::new(source)) {
                        source_ids.extend(indexes.collect_active_sorted(self));
                    }
                }
                source_ids
            }
            (None, Some(sink_key)) => self
                .by_sink
                .get(sink_key)
                .map(|indexes| indexes.collect_active_sorted(self))
                .unwrap_or_default()
                .into_iter()
                .collect::<HashSet<_>>(),
            (None, None) => self
                .entries
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| entry.active.then_some(index))
                .collect(),
        };
        let mut sorted = ids.into_iter().collect::<Vec<_>>();
        sorted.sort_unstable();
        sorted
    }
}
