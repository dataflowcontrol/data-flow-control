use roaring::RoaringBitmap;
use smallvec::SmallVec;

use crate::policy_store::PolicyStore;

/// Switch dense per-source indexes from sorted vectors to compressed bitmaps.
pub(crate) const BITMAP_INDEX_THRESHOLD: usize = 512;

/// Sorted policy-id index that compresses to a Roaring bitmap for dense sources.
#[derive(Debug, Clone)]
pub(crate) enum PolicyIndex {
    List(Vec<usize>),
    Bitmap(RoaringBitmap),
}

impl Default for PolicyIndex {
    fn default() -> Self {
        Self::List(Vec::new())
    }
}

impl PolicyIndex {
    pub(crate) fn push_id(&mut self, id: usize) {
        match self {
            Self::List(list) => {
                list.push(id);
                if list.len() >= BITMAP_INDEX_THRESHOLD {
                    *self = Self::Bitmap(bitmap_from_sorted_list(list));
                }
            }
            Self::Bitmap(bitmap) => {
                bitmap.insert(u32::try_from(id).expect("policy id exceeds u32"));
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Self::List(list) => list.len(),
            Self::Bitmap(bitmap) => bitmap.len() as usize,
        }
    }

    pub(crate) fn is_bitmap(&self) -> bool {
        matches!(self, Self::Bitmap(_))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn iter_active<'a>(&'a self, store: &'a PolicyStore) -> PolicyIndexActiveIter<'a> {
        match self {
            Self::List(list) => PolicyIndexActiveIter::List {
                store,
                list,
                pos: 0,
            },
            Self::Bitmap(bitmap) => PolicyIndexActiveIter::Bitmap {
                store,
                bitmap,
                cursor: PolicyBitmapCursor::new(bitmap),
                pending: None,
            },
        }
    }

    pub(crate) fn collect_active_sorted(&self, store: &PolicyStore) -> Vec<usize> {
        self.iter_active(store).collect()
    }
}

fn bitmap_from_sorted_list(ids: &[usize]) -> RoaringBitmap {
    let mut bitmap = RoaringBitmap::new();
    for &id in ids {
        bitmap.insert(u32::try_from(id).expect("policy id exceeds u32"));
    }
    bitmap
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PolicyBitmapCursor {
    next_id: Option<u32>,
}

impl PolicyBitmapCursor {
    fn new(bitmap: &RoaringBitmap) -> Self {
        Self {
            next_id: bitmap.iter().next(),
        }
    }

    fn next_active_id(&mut self, bitmap: &RoaringBitmap, store: &PolicyStore) -> Option<usize> {
        while let Some(id) = self.next_id {
            self.next_id = bitmap.range(id + 1..).next();
            let index = id as usize;
            if store.entries.get(index).is_some_and(|entry| entry.active) {
                return Some(index);
            }
        }
        None
    }
}

/// Iterate active policy ids from a single index without materializing a vector.
pub(crate) enum PolicyIndexActiveIter<'a> {
    List {
        store: &'a PolicyStore,
        list: &'a [usize],
        pos: usize,
    },
    Bitmap {
        store: &'a PolicyStore,
        bitmap: &'a RoaringBitmap,
        cursor: PolicyBitmapCursor,
        pending: Option<usize>,
    },
}

impl Iterator for PolicyIndexActiveIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::List { store, list, pos } => {
                while *pos < list.len() {
                    let id = list[*pos];
                    *pos += 1;
                    if store.entries.get(id).is_some_and(|entry| entry.active) {
                        return Some(id);
                    }
                }
                None
            }
            Self::Bitmap {
                store,
                bitmap,
                cursor,
                pending,
            } => {
                if pending.is_none() {
                    *pending = cursor.next_active_id(bitmap, store);
                }
                pending.take()
            }
        }
    }
}

/// Owns index-list references for a k-way merge over one lookup scope.
pub(crate) struct MergedPolicyIndexView<'a> {
    store: &'a PolicyStore,
    lists: SmallVec<[&'a PolicyIndex; 16]>,
}

impl<'a> MergedPolicyIndexView<'a> {
    pub(crate) fn new(store: &'a PolicyStore, lists: SmallVec<[&'a PolicyIndex; 16]>) -> Self {
        Self { store, lists }
    }

    pub(crate) fn iter(&self) -> PolicyIndexMergeIter<'_> {
        PolicyIndexMergeIter::new(self.store, &self.lists)
    }

    pub(crate) fn collect_ids(self) -> Vec<usize> {
        self.iter().collect()
    }
}

/// K-way merge iterator over sorted policy-id indexes, skipping inactive entries.
pub(crate) struct PolicyIndexMergeIter<'a> {
    cursors: SmallVec<[PolicyIndexCursor<'a>; 8]>,
    last: Option<usize>,
}

impl<'a> PolicyIndexMergeIter<'a> {
    pub(crate) fn new(store: &'a PolicyStore, lists: &'a [&'a PolicyIndex]) -> Self {
        if lists.is_empty() {
            return Self {
                cursors: SmallVec::new(),
                last: None,
            };
        }
        if lists.len() == 1 {
            return Self {
                cursors: SmallVec::from_iter([PolicyIndexCursor::new(store, lists[0])]),
                last: None,
            };
        }
        Self {
            cursors: lists
                .iter()
                .map(|index| PolicyIndexCursor::new(store, index))
                .collect(),
            last: None,
        }
    }
}

impl Iterator for PolicyIndexMergeIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let mut min_id = None;
            let mut min_list = None;
            for (list_idx, cursor) in self.cursors.iter_mut().enumerate() {
                if let Some(id) = cursor.peek_active()
                    && min_id.is_none_or(|current| id < current)
                {
                    min_id = Some(id);
                    min_list = Some(list_idx);
                }
            }
            let min_id = min_id?;
            self.cursors[min_list.expect("min list index")].advance(min_id);
            if self.last != Some(min_id) {
                self.last = Some(min_id);
                return Some(min_id);
            }
        }
    }
}

enum PolicyIndexCursorState<'a> {
    List {
        list: &'a [usize],
        pos: usize,
    },
    Bitmap {
        bitmap: &'a RoaringBitmap,
        cursor: PolicyBitmapCursor,
        pending: Option<usize>,
    },
}

struct PolicyIndexCursor<'a> {
    store: &'a PolicyStore,
    state: PolicyIndexCursorState<'a>,
}

impl<'a> PolicyIndexCursor<'a> {
    fn new(store: &'a PolicyStore, index: &'a PolicyIndex) -> Self {
        let state = match index {
            PolicyIndex::List(list) => PolicyIndexCursorState::List { list, pos: 0 },
            PolicyIndex::Bitmap(bitmap) => PolicyIndexCursorState::Bitmap {
                bitmap,
                cursor: PolicyBitmapCursor::new(bitmap),
                pending: None,
            },
        };
        Self { store, state }
    }

    fn peek_active(&mut self) -> Option<usize> {
        match &mut self.state {
            PolicyIndexCursorState::List { list, pos } => {
                let mut scan = *pos;
                while scan < list.len() {
                    let id = list[scan];
                    if self.store.entries.get(id).is_some_and(|entry| entry.active) {
                        return Some(id);
                    }
                    scan += 1;
                }
                None
            }
            PolicyIndexCursorState::Bitmap {
                bitmap,
                cursor,
                pending,
            } => {
                if pending.is_none() {
                    *pending = cursor.next_active_id(bitmap, self.store);
                }
                *pending
            }
        }
    }

    fn advance(&mut self, current_id: usize) {
        match &mut self.state {
            PolicyIndexCursorState::List { list, pos } => {
                if *pos < list.len() && list[*pos] == current_id {
                    *pos += 1;
                    return;
                }
                while *pos < list.len() && list[*pos] <= current_id {
                    *pos += 1;
                }
            }
            PolicyIndexCursorState::Bitmap { pending, .. } => {
                debug_assert_eq!(*pending, Some(current_id));
                *pending = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roaring_bitmap_index_promotes_at_threshold() {
        let mut store = PolicyStore::default();
        for index in 0..600usize {
            store.register(crate::policy::PolicyIr::Pgn {
                sources: vec!["orders".to_string()],
                required_sources: Vec::new(),
                dimension_tables: Vec::new(),
                dimension_aliases: std::collections::HashMap::new(),
                dimension_queries: std::collections::HashMap::new(),
                sink: None,
                sink_alias: None,
                source_aliases: std::collections::HashMap::new(),
                constraint: format!("max(orders.amount) > {index}"),
                on_fail: crate::policy::Resolution::Remove,
                description: None,
            });
        }
        let hot = crate::identifiers::TableKey::new("orders");
        assert_eq!(store.policies_for_source(&hot).len(), 600);
        let usage = store.memory_usage();
        assert!(usage.source_bitmap_indexes >= 1);
        assert!(usage.source_index_estimated_bytes < 600 * std::mem::size_of::<usize>());
    }
}
