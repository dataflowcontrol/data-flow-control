use serde::{Deserialize, Serialize};

use super::PolicyStore;
use crate::policy_index::PolicyIndex;

fn index_memory_bytes(index: &PolicyIndex) -> usize {
    match index {
        PolicyIndex::List(list) => list.len() * std::mem::size_of::<usize>(),
        PolicyIndex::Bitmap(bitmap) => bitmap.serialized_size(),
    }
}

/// Estimated in-memory footprint for a registered policy store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyStoreMemoryUsage {
    pub entry_count: usize,
    pub active_entries: usize,
    pub compiled_constraint_shared_bytes: usize,
    pub unique_constraint_strings: usize,
    pub unique_column_keys: usize,
    pub referenced_column_pairs: usize,
    pub source_index_count: usize,
    pub source_bitmap_indexes: usize,
    pub source_index_estimated_bytes: usize,
    pub table_key_cache_bytes: usize,
    pub column_key_cache_bytes: usize,
    pub interned_string_key_bytes: usize,
}

impl PolicyStoreMemoryUsage {
    /// Sum of tracked index and intern-cache byte estimates (excludes `Arc`/`CompiledPolicy` bodies).
    pub fn indexed_metadata_bytes(&self) -> usize {
        self.source_index_estimated_bytes
            + self.table_key_cache_bytes
            + self.column_key_cache_bytes
            + self.interned_string_key_bytes
    }
}

impl PolicyStore {
    pub fn memory_usage(&self) -> PolicyStoreMemoryUsage {
        let mut source_index_estimated_bytes = 0usize;
        let mut source_bitmap_indexes = 0usize;
        for index in self.by_source.values() {
            source_index_estimated_bytes += index_memory_bytes(index);
            if index.is_bitmap() {
                source_bitmap_indexes += 1;
            }
        }
        for index in self
            .by_sink
            .values()
            .chain(self.enforcement_by_source.values())
            .chain(self.join_pushdown_by_source.values())
        {
            source_index_estimated_bytes += index_memory_bytes(index);
            if index.is_bitmap() {
                source_bitmap_indexes += 1;
            }
        }
        source_index_estimated_bytes += self.multi_source.estimated_bytes();
        source_index_estimated_bytes += self.enforcement_multi_source.estimated_bytes();
        source_index_estimated_bytes += index_memory_bytes(&self.global_no_source);
        source_index_estimated_bytes += index_memory_bytes(&self.enforcement_global_no_source);

        let compiled_constraint_shared_bytes = self
            .entries
            .iter()
            .filter_map(|entry| entry.constraint.as_ref())
            .map(|constraint| constraint.source_sql.len())
            .sum::<usize>();
        let referenced_column_pairs = self
            .entries
            .iter()
            .map(|entry| entry.constraint_referenced_columns.len())
            .sum::<usize>();

        PolicyStoreMemoryUsage {
            entry_count: self.entries.len(),
            active_entries: self.active_count(),
            compiled_constraint_shared_bytes,
            unique_constraint_strings: self.constraint_intern.unique_count(),
            unique_column_keys: self.column_key_cache.len(),
            referenced_column_pairs,
            source_index_count: self.by_source.len(),
            source_bitmap_indexes,
            source_index_estimated_bytes,
            table_key_cache_bytes: self.table_key_cache.keys().map(|key| key.len()).sum(),
            column_key_cache_bytes: self.column_key_cache.keys().map(|key| key.len()).sum(),
            interned_string_key_bytes: self.constraint_intern.retained_key_bytes(),
        }
    }
}
