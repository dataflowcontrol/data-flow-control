use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use sqlparser::ast::Expr;

use crate::diagnostics::RewriteError;
use crate::identifiers::{ColumnKey, TableKey, normalize_key};
use crate::intern::StringInterner;
use crate::policy::{PolicyIr, Resolution};
use crate::policy_index::{MergedPolicyIndexView, PolicyIndex, merge_policy_indexes};
use crate::semiring::{AggregateAnalysis, SemiringAnalysis, analyze_constraint};
use crate::source_set_index::SourceSetPolicyIndex;
use crate::source_sets::{
    compile_constraint_referenced_source_keys, compile_source_local_conjuncts,
};
use crate::sql::{collect_qualified_columns_from_expr, parse_projection_expr};
use crate::threshold::{ThresholdPredicate, threshold_predicate_from_policy};

/// How multi-source policies are matched against visible query tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MultiSourceLookupMode {
    /// Include a multi-source policy only when all of its sources are visible.
    #[default]
    Subset,
    /// Include a multi-source policy when any source is visible (partial-push / branch-local).
    AnyOverlap,
}

/// Parsed constraint metadata compiled once at policy registration.
#[derive(Debug, Clone)]
pub struct CompiledExpr {
    pub source_sql: Arc<str>,
    pub ast: Expr,
}

/// Registration-time compiled policy used by the rewrite path.
#[derive(Debug, Clone)]
pub struct CompiledPolicy {
    pub index: usize,
    pub policy: PolicyIr,
    pub active: bool,
    pub constraint: Option<CompiledExpr>,
    pub semiring: SemiringAnalysis,
    pub source_keys: SmallVec<[TableKey; 4]>,
    pub required_source_keys: SmallVec<[TableKey; 4]>,
    pub sink_key: Option<TableKey>,
    pub(crate) threshold: Option<ThresholdPredicate>,
    pub join_pushdown_eligible: bool,
    /// Precomputed `transform_scan_aggregates` for distributive aggregate constraints.
    pub(crate) scan_ready_expr: Option<Expr>,
    /// Pre-split AND conjuncts keyed by source for multi-source enforcement policies.
    pub(crate) source_local_conjuncts: Option<SmallVec<[(TableKey, Expr); 4]>>,
    /// Source tables referenced in the constraint expression.
    pub(crate) constraint_referenced_sources: SmallVec<[TableKey; 4]>,
    /// Qualified columns referenced in the constraint/dimensions, interned at registration.
    pub(crate) constraint_referenced_columns: SmallVec<[(TableKey, ColumnKey); 4]>,
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
    /// Multi-source enforcement policies (REMOVE/KILL/LLM) indexed by source set.
    enforcement_multi_source: SourceSetPolicyIndex,
    /// Multi-source policies indexed by canonical source set.
    multi_source: SourceSetPolicyIndex,
    aggregate_policies: Vec<usize>,
    aggregate_by_source: HashMap<TableKey, PolicyIndex>,
    aggregate_by_sink: HashMap<TableKey, PolicyIndex>,
    aggregate_no_sink: PolicyIndex,
    /// Multi-source aggregate policies indexed by source set (not duplicated per source).
    aggregate_multi_source: SourceSetPolicyIndex,
    dfc_policies: Vec<usize>,
    pgn_policies: Vec<usize>,
    remove_policy_count: usize,
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
        let index = self.entries.len();
        let compiled = Arc::new(self.compile_policy(index, policy));
        self.index_entry(&compiled);
        self.entries.push(compiled);
        self.active_policy_count += 1;
        index
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
            .chain(self.aggregate_by_source.values())
            .chain(self.aggregate_by_sink.values())
        {
            source_index_estimated_bytes += index_memory_bytes(index);
            if index.is_bitmap() {
                source_bitmap_indexes += 1;
            }
        }
        source_index_estimated_bytes += self.multi_source.estimated_bytes();
        source_index_estimated_bytes += self.enforcement_multi_source.estimated_bytes();
        source_index_estimated_bytes += self.aggregate_multi_source.estimated_bytes();
        source_index_estimated_bytes += index_memory_bytes(&self.global_no_source);
        source_index_estimated_bytes += index_memory_bytes(&self.enforcement_global_no_source);
        source_index_estimated_bytes += index_memory_bytes(&self.aggregate_no_sink);

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

    pub fn deactivate(&mut self, index: usize) -> bool {
        let Some(entry) = self.entries.get_mut(index) else {
            return false;
        };
        if !entry.active {
            return false;
        }
        if entry.policy.resolution() == Resolution::Remove {
            self.remove_policy_count = self.remove_policy_count.saturating_sub(1);
        }
        let updated = Arc::new(CompiledPolicy {
            active: false,
            ..(**entry).clone()
        });
        *entry = updated;
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

    pub fn semiring_for_candidates(&self, candidate_ids: &[usize]) -> SemiringAnalysis {
        self.semiring_for_candidate_iter(candidate_ids.iter().copied())
    }

    pub(crate) fn semiring_for_candidate_iter(
        &self,
        candidate_ids: impl Iterator<Item = usize>,
    ) -> SemiringAnalysis {
        merge_semiring(candidate_ids.filter_map(|index| self.compiled(index)))
    }

    pub fn candidate_ids_for_tables(&self, tables: &HashSet<TableKey>) -> Vec<usize> {
        self.candidate_ids_for_scope(tables, None)
    }

    pub fn candidate_ids_for_scope(
        &self,
        tables: &HashSet<TableKey>,
        sink: Option<&TableKey>,
    ) -> Vec<usize> {
        self.candidate_ids_for_scope_with_mode(tables, sink, MultiSourceLookupMode::Subset)
    }

    /// Candidate lookup allowing partial multi-source visibility (partial-push / set-op branches).
    pub fn candidate_ids_for_scope_with_overlap(
        &self,
        tables: &HashSet<TableKey>,
        sink: Option<&TableKey>,
    ) -> Vec<usize> {
        self.candidate_ids_for_scope_with_mode(tables, sink, MultiSourceLookupMode::AnyOverlap)
    }

    pub fn candidate_ids_for_scope_with_mode(
        &self,
        tables: &HashSet<TableKey>,
        sink: Option<&TableKey>,
        multi_source_mode: MultiSourceLookupMode,
    ) -> Vec<usize> {
        self.candidate_scope_lookup(tables, sink, multi_source_mode)
            .collect_ids()
    }

    /// View over merged candidate indexes for streaming iteration without allocating ids.
    pub(crate) fn candidate_scope_lookup<'a>(
        &'a self,
        tables: &'a HashSet<TableKey>,
        sink: Option<&'a TableKey>,
        multi_source_mode: MultiSourceLookupMode,
    ) -> MergedPolicyIndexView<'a> {
        MergedPolicyIndexView::new(
            self,
            self.candidate_scope_index_lists(tables, sink, multi_source_mode),
        )
    }

    fn candidate_scope_index_lists<'a>(
        &'a self,
        tables: &'a HashSet<TableKey>,
        sink: Option<&'a TableKey>,
        multi_source_mode: MultiSourceLookupMode,
    ) -> SmallVec<[&'a PolicyIndex; 16]> {
        let mut lists = SmallVec::<[&PolicyIndex; 16]>::new();
        for table in tables {
            if let Some(source_ids) = self.by_source.get(table) {
                lists.push(source_ids);
            }
        }
        if let Some(sink_ids) = sink.and_then(|sink| self.by_sink.get(sink)) {
            lists.push(sink_ids);
        }
        if !self.global_no_source.is_empty() {
            lists.push(&self.global_no_source);
        }
        for index in self.multi_source.index_lists_for(tables, multi_source_mode) {
            lists.push(index);
        }
        lists
    }

    pub(crate) fn enforcement_candidate_lookup<'a>(
        &'a self,
        tables: &'a HashSet<TableKey>,
    ) -> MergedPolicyIndexView<'a> {
        let mut lists = SmallVec::<[&PolicyIndex; 16]>::new();
        for table in tables {
            if let Some(source_ids) = self.enforcement_by_source.get(table) {
                lists.push(source_ids);
            }
        }
        if !self.enforcement_global_no_source.is_empty() {
            lists.push(&self.enforcement_global_no_source);
        }
        for index in self
            .enforcement_multi_source
            .index_lists_for(tables, MultiSourceLookupMode::AnyOverlap)
        {
            lists.push(index);
        }
        MergedPolicyIndexView::new(self, lists)
    }

    pub(crate) fn aggregate_scan_policy_lookup<'a>(
        &'a self,
        visible_sources: &'a HashSet<TableKey>,
    ) -> MergedPolicyIndexView<'a> {
        MergedPolicyIndexView::new(self, self.aggregate_scan_index_lists(visible_sources))
    }

    fn aggregate_scan_index_lists<'a>(
        &'a self,
        visible_sources: &'a HashSet<TableKey>,
    ) -> SmallVec<[&'a PolicyIndex; 16]> {
        let mut lists = SmallVec::<[&PolicyIndex; 16]>::new();
        if !self.aggregate_no_sink.is_empty() {
            lists.push(&self.aggregate_no_sink);
        }
        for source in visible_sources {
            if let Some(index) = self.aggregate_by_source.get(source) {
                lists.push(index);
            }
        }
        for index in self
            .aggregate_multi_source
            .index_lists_for(visible_sources, MultiSourceLookupMode::Subset)
        {
            lists.push(index);
        }
        lists
    }

    pub fn join_pushdown_candidates(&self, source: &TableKey) -> Vec<usize> {
        self.join_pushdown_by_source
            .get(source)
            .map(|ids| ids.collect_active_sorted(self))
            .unwrap_or_default()
    }

    /// Candidate enforcement policies (REMOVE/KILL/LLM) reachable from visible tables.
    pub fn enforcement_candidate_ids_for_tables(&self, tables: &HashSet<TableKey>) -> Vec<usize> {
        self.enforcement_candidate_lookup(tables).collect_ids()
    }

    pub fn clone_constraint_ast(&self, index: usize) -> Option<Expr> {
        self.compiled(index).and_then(|entry| {
            entry
                .constraint
                .as_ref()
                .map(|compiled| compiled.ast.clone())
        })
    }

    /// Drop dominated REMOVE threshold policies using registration-time threshold metadata.
    pub fn prune_dominated_candidates(&self, candidate_ids: &[usize]) -> (Vec<usize>, usize) {
        let mut keep = vec![true; candidate_ids.len()];
        let mut strongest_by_key: HashMap<crate::threshold::ThresholdKey, usize> = HashMap::new();

        for (slot, &index) in candidate_ids.iter().enumerate() {
            let Some(compiled) = self.compiled(index) else {
                keep[slot] = false;
                continue;
            };
            let Some(candidate) = compiled.threshold.as_ref() else {
                continue;
            };
            if let Some(existing_slot) = strongest_by_key.get(&candidate.key).copied() {
                let Some(existing) = self
                    .compiled(candidate_ids[existing_slot])
                    .and_then(|entry| entry.threshold.as_ref())
                else {
                    continue;
                };
                if crate::threshold::threshold_dominates_predicates(existing, candidate) {
                    keep[slot] = false;
                    continue;
                }
                if crate::threshold::threshold_dominates_predicates(candidate, existing) {
                    keep[existing_slot] = false;
                    strongest_by_key.insert(candidate.key.clone(), slot);
                }
            } else {
                strongest_by_key.insert(candidate.key.clone(), slot);
            }
        }

        let dominated = keep.iter().filter(|&&k| !k).count();
        let kept = candidate_ids
            .iter()
            .copied()
            .zip(keep)
            .filter_map(|(index, k)| k.then_some(index))
            .collect();
        (kept, dominated)
    }

    pub fn policies_for_source(&self, source: &TableKey) -> Vec<usize> {
        self.by_source
            .get(source)
            .map(|ids| ids.collect_active_sorted(self))
            .unwrap_or_default()
    }

    pub fn candidate_ids_for_sink(&self, sink: &TableKey) -> Vec<usize> {
        self.by_sink
            .get(sink)
            .map(|ids| ids.collect_active_sorted(self))
            .unwrap_or_default()
    }

    pub fn aggregate_policy_indices(&self) -> Vec<usize> {
        self.active_ids(&self.aggregate_policies).collect()
    }

    pub fn aggregate_policy_indices_for_sink(&self, sink: &str) -> Vec<usize> {
        let sink_key = TableKey::new(sink);
        let mut lists = Vec::new();
        if let Some(index) = self.aggregate_by_sink.get(&sink_key) {
            lists.push(index);
        }
        if !self.aggregate_no_sink.is_empty() {
            lists.push(&self.aggregate_no_sink);
        }
        let multi_sink = self.aggregate_multi_source_ids_for_sink(&sink_key);
        if !multi_sink.is_empty() {
            lists.push(&multi_sink);
        }
        merge_policy_indexes(self, &lists)
    }

    fn aggregate_multi_source_ids_for_sink(&self, sink: &TableKey) -> PolicyIndex {
        let ids = self
            .aggregate_multi_source
            .active_policy_ids(self)
            .filter(|&index| {
                self.compiled(index)
                    .is_some_and(|entry| entry.sink_key.as_ref() == Some(sink))
            })
            .collect::<Vec<_>>();
        PolicyIndex::List(ids)
    }

    pub fn aggregate_policy_indices_for_scope(
        &self,
        sink: &str,
        visible_sources: &HashSet<TableKey>,
    ) -> Vec<usize> {
        let mut lists = Vec::new();
        let sink_key = TableKey::new(sink);
        if let Some(index) = self.aggregate_by_sink.get(&sink_key) {
            lists.push(index);
        }
        if !self.aggregate_no_sink.is_empty() {
            lists.push(&self.aggregate_no_sink);
        }
        for source in visible_sources {
            if let Some(index) = self.aggregate_by_source.get(source) {
                lists.push(index);
            }
        }
        lists.extend(
            self.aggregate_multi_source
                .index_lists_for(visible_sources, MultiSourceLookupMode::Subset),
        );
        merge_policy_indexes(self, &lists)
    }

    /// Aggregate policies applicable during SELECT scan rewrites (no sink).
    pub fn aggregate_scan_policy_indices(&self, visible_sources: &HashSet<TableKey>) -> Vec<usize> {
        self.aggregate_scan_policy_lookup(visible_sources)
            .collect_ids()
    }

    pub fn multi_source_policy_indices(&self) -> Vec<usize> {
        self.multi_source.active_policy_ids(self).collect()
    }

    pub fn dfc_policy_indices(&self) -> Vec<usize> {
        self.active_ids(&self.dfc_policies).collect()
    }

    pub fn pgn_policy_indices(&self) -> Vec<usize> {
        self.active_ids(&self.pgn_policies).collect()
    }

    pub fn scan_ready_expr(&self, index: usize) -> Option<Expr> {
        self.compiled(index)
            .and_then(|entry| entry.scan_ready_expr.clone())
    }

    pub(crate) fn set_scan_ready_expr(&mut self, index: usize, scan_ready: Expr) {
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        *entry = Arc::new(CompiledPolicy {
            scan_ready_expr: Some(scan_ready),
            ..(**entry).clone()
        });
    }

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
                    if let Some(indexes) = self.by_source.get(&TableKey::new(source)) {
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
            (None, Some(sink_key)) => {
                let mut ids = self
                    .by_sink
                    .get(sink_key)
                    .map(|indexes| indexes.collect_active_sorted(self))
                    .unwrap_or_default();
                if let Some(aggregate_ids) = self.aggregate_by_sink.get(sink_key) {
                    ids = merge_policy_indexes(self, &[&PolicyIndex::List(ids), aggregate_ids]);
                }
                ids.into_iter().collect::<HashSet<_>>()
            }
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

    pub fn clone_policies_for_candidates(&self, candidate_ids: &[usize]) -> Vec<PolicyIr> {
        candidate_ids
            .iter()
            .filter_map(|&index| self.policy(index).cloned())
            .collect()
    }

    pub fn clone_policies_for_scope(
        &self,
        tables: &HashSet<TableKey>,
        sink: Option<&TableKey>,
    ) -> Vec<PolicyIr> {
        self.candidate_entries_for_scope(tables, sink)
            .into_iter()
            .map(|(_, policy)| policy)
            .collect()
    }

    pub fn candidate_entries_for_scope(
        &self,
        tables: &HashSet<TableKey>,
        sink: Option<&TableKey>,
    ) -> Vec<(usize, PolicyIr)> {
        self.candidate_ids_for_scope(tables, sink)
            .into_iter()
            .filter_map(|index| self.policy(index).cloned().map(|policy| (index, policy)))
            .collect()
    }

    /// Debug/test validation: indexed lookup should match slow all-policy scan.
    pub fn assert_candidates_match_slow_scan(
        &self,
        tables: &HashSet<TableKey>,
        sink: Option<&TableKey>,
        multi_source_mode: MultiSourceLookupMode,
    ) {
        let indexed: Vec<_> = self
            .candidate_scope_lookup(tables, sink, multi_source_mode)
            .iter()
            .collect();
        let slow = self.slow_candidate_ids_for_scope(tables, sink, multi_source_mode);
        assert_eq!(
            indexed, slow,
            "indexed candidate lookup diverged from slow scan (mode={multi_source_mode:?})"
        );
    }

    pub(crate) fn debug_assert_candidates_match_slow_scan(
        &self,
        tables: &HashSet<TableKey>,
        sink: Option<&TableKey>,
        multi_source_mode: MultiSourceLookupMode,
    ) {
        #[cfg(feature = "validate-indexed-candidates")]
        {
            let indexed: Vec<_> = self
                .candidate_scope_lookup(tables, sink, multi_source_mode)
                .iter()
                .collect();
            debug_assert_eq!(
                indexed,
                self.slow_candidate_ids_for_scope(tables, sink, multi_source_mode),
                "indexed candidate lookup diverged from slow scan"
            );
        }
        #[cfg(not(feature = "validate-indexed-candidates"))]
        let _ = (tables, sink, multi_source_mode);
    }

    pub fn has_any_remove_policies(&self) -> bool {
        self.remove_policy_count > 0
    }

    pub(crate) fn constraint_expr(
        &self,
        index: usize,
        fallback: &str,
        stats: Option<&crate::rewrite_stats::RewriteStatsCell>,
    ) -> Result<Expr, RewriteError> {
        if let Some(ast) = self.clone_constraint_ast(index) {
            return Ok(ast);
        }
        if let Some(stats) = stats {
            stats.record_constraint_parse();
        }
        parse_projection_expr(fallback)
    }

    /// Debug/test validation: scan every active policy and filter by table overlap.
    pub fn slow_candidate_ids_for_tables(&self, tables: &HashSet<TableKey>) -> Vec<usize> {
        self.slow_candidate_ids_for_scope(tables, None, MultiSourceLookupMode::Subset)
    }

    pub fn slow_candidate_ids_for_scope(
        &self,
        tables: &HashSet<TableKey>,
        sink: Option<&TableKey>,
        multi_source_mode: MultiSourceLookupMode,
    ) -> Vec<usize> {
        let mut ids = HashSet::new();
        for (index, entry) in self.entries.iter().enumerate() {
            if !entry.active {
                continue;
            }
            if matches!(entry.policy, PolicyIr::CompatAggregate(_)) {
                continue;
            }
            if entry.source_keys.is_empty() {
                if entry.sink_key.is_none() {
                    ids.insert(index);
                }
                continue;
            }
            let source_visible = if entry.source_keys.len() > 1 {
                match multi_source_mode {
                    MultiSourceLookupMode::Subset => entry
                        .source_keys
                        .iter()
                        .all(|source| tables.contains(source)),
                    MultiSourceLookupMode::AnyOverlap => entry
                        .source_keys
                        .iter()
                        .any(|source| tables.contains(source)),
                }
            } else {
                tables.contains(&entry.source_keys[0])
            };
            if source_visible {
                ids.insert(index);
            }
        }
        if let Some(sink_key) = sink {
            for (index, entry) in self.entries.iter().enumerate() {
                if entry.active && entry.sink_key.as_ref() == Some(sink_key) {
                    ids.insert(index);
                }
            }
        }
        let mut sorted = ids.into_iter().collect::<Vec<_>>();
        sorted.sort_unstable();
        sorted
    }

    fn active_ids<'a>(&'a self, ids: &'a [usize]) -> impl Iterator<Item = usize> + 'a {
        ids.iter()
            .copied()
            .filter(|&index| self.entries.get(index).is_some_and(|entry| entry.active))
    }

    fn index_entry(&mut self, entry: &CompiledPolicy) {
        let index = entry.index;
        let is_aggregate = matches!(entry.policy, PolicyIr::CompatAggregate(_));

        if !is_aggregate {
            if entry.source_keys.is_empty() {
                if entry.sink_key.is_none() {
                    self.global_no_source.push_id(index);
                }
            } else if entry.source_keys.len() == 1 {
                self.by_source
                    .entry(entry.source_keys[0].clone())
                    .or_default()
                    .push_id(index);
            } else {
                self.multi_source.register(&entry.source_keys, index);
            }
            if is_enforcement_resolution(entry.policy.resolution()) {
                if entry.source_keys.is_empty() {
                    if entry.sink_key.is_none() {
                        self.enforcement_global_no_source.push_id(index);
                    }
                } else if entry.source_keys.len() == 1 {
                    self.enforcement_by_source
                        .entry(entry.source_keys[0].clone())
                        .or_default()
                        .push_id(index);
                } else {
                    self.enforcement_multi_source
                        .register(&entry.source_keys, index);
                }
            }
            if let Some(sink) = &entry.sink_key {
                self.by_sink.entry(sink.clone()).or_default().push_id(index);
            }
            if entry.join_pushdown_eligible && entry.source_keys.len() == 1 {
                self.join_pushdown_by_source
                    .entry(entry.source_keys[0].clone())
                    .or_default()
                    .push_id(index);
            }
        }
        if is_aggregate {
            self.aggregate_policies.push(index);
            if let Some(sink) = &entry.sink_key {
                if entry.source_keys.len() <= 1 {
                    self.aggregate_by_sink
                        .entry(sink.clone())
                        .or_default()
                        .push_id(index);
                }
            } else {
                self.aggregate_no_sink.push_id(index);
            }
            if entry.source_keys.len() == 1 {
                self.aggregate_by_source
                    .entry(entry.source_keys[0].clone())
                    .or_default()
                    .push_id(index);
            } else if entry.source_keys.len() > 1 {
                self.aggregate_multi_source
                    .register(&entry.source_keys, index);
            }
        }
        match &entry.policy {
            PolicyIr::CompatDfc { .. } => self.dfc_policies.push(index),
            PolicyIr::NativePgn(_) => self.pgn_policies.push(index),
            _ => {}
        }
        if entry.policy.resolution() == Resolution::Remove {
            self.remove_policy_count += 1;
        }
    }
}

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

fn is_enforcement_resolution(resolution: Resolution) -> bool {
    matches!(
        resolution,
        Resolution::Remove | Resolution::Kill | Resolution::Llm
    )
}

impl PolicyStore {
    fn compile_policy(&mut self, index: usize, policy: PolicyIr) -> CompiledPolicy {
        let constraint_sql = self.intern_string(policy.constraint());
        let constraint = parse_projection_expr(constraint_sql.as_ref())
            .ok()
            .map(|ast| CompiledExpr {
                source_sql: constraint_sql.clone(),
                ast,
            });
        let semiring = semiring_for_constraint(constraint_sql.as_ref());
        let source_keys = policy
            .sources()
            .iter()
            .map(|source| self.intern_table_key(source))
            .collect::<SmallVec<[TableKey; 4]>>();
        let required_source_keys = policy
            .required_sources()
            .iter()
            .map(|source| self.intern_table_key(source))
            .collect::<SmallVec<[TableKey; 4]>>();
        let sink_key = policy.sink().map(|sink| self.intern_table_key(sink));
        let threshold = threshold_predicate_from_policy(&policy);
        let join_pushdown_eligible = matches!(
            &policy,
            PolicyIr::CompatDfc {
                sources,
                required_sources,
                sink: None,
                on_fail: Resolution::Remove | Resolution::Kill | Resolution::Llm,
                ..
            } if required_sources.is_empty() && sources.len() == 1
        );
        let (source_local_conjuncts, constraint_referenced_sources, constraint_referenced_columns) =
            if let Some(compiled) = constraint.as_ref() {
                let referenced =
                    compile_constraint_referenced_source_keys(&compiled.ast, &source_keys);
                let mut columns = compile_constraint_referenced_column_keys(self, &compiled.ast);
                for dimension in policy.dimensions() {
                    if let Ok(expr) = parse_projection_expr(dimension) {
                        columns.extend(compile_constraint_referenced_column_keys(self, &expr));
                    }
                }
                let columns = dedup_referenced_column_keys(columns);
                let conjuncts = if matches!(
                    policy.resolution(),
                    Resolution::Remove | Resolution::Kill | Resolution::Llm
                ) && policy.sink().is_none()
                    && policy.required_sources().is_empty()
                    && policy.dimensions().is_empty()
                    && source_keys.len() > 1
                {
                    compile_source_local_conjuncts(&compiled.ast, &source_keys)
                } else {
                    None
                };
                (conjuncts, referenced, columns)
            } else {
                (None, SmallVec::new(), SmallVec::new())
            };

        CompiledPolicy {
            index,
            policy,
            active: true,
            constraint,
            semiring,
            source_keys,
            required_source_keys,
            sink_key,
            threshold,
            join_pushdown_eligible,
            scan_ready_expr: None,
            source_local_conjuncts,
            constraint_referenced_sources,
            constraint_referenced_columns,
        }
    }
}

fn compile_constraint_referenced_column_keys(
    store: &mut PolicyStore,
    expr: &Expr,
) -> SmallVec<[(TableKey, ColumnKey); 4]> {
    let mut pairs = SmallVec::new();
    for column in collect_qualified_columns_from_expr(expr) {
        let table = store.intern_table_key(column.table.as_str());
        let column_key = store.intern_column_key(column.column.as_str());
        pairs.push((table, column_key));
    }
    pairs
}

fn dedup_referenced_column_keys(
    pairs: SmallVec<[(TableKey, ColumnKey); 4]>,
) -> SmallVec<[(TableKey, ColumnKey); 4]> {
    let mut seen = HashSet::new();
    let mut deduped: SmallVec<[(TableKey, ColumnKey); 4]> = SmallVec::new();
    for (table, column) in pairs {
        if seen.insert((table.clone(), column.clone())) {
            deduped.push((table, column));
        }
    }
    deduped.sort_by(|left, right| {
        left.0
            .as_str()
            .cmp(right.0.as_str())
            .then_with(|| left.1.as_str().cmp(right.1.as_str()))
    });
    deduped
}

fn semiring_for_constraint(constraint: &str) -> SemiringAnalysis {
    match analyze_constraint(constraint) {
        Ok(aggregates) => merge_semiring_from_aggregates(aggregates),
        Err(_) => SemiringAnalysis {
            aggregate_count: 0,
            all_distributive: false,
            non_distributive_aggregates: vec![format!("unparseable::{constraint}")],
        },
    }
}

fn merge_semiring<'a>(entries: impl Iterator<Item = &'a CompiledPolicy>) -> SemiringAnalysis {
    let mut result = SemiringAnalysis::default();
    for entry in entries {
        result.aggregate_count += entry.semiring.aggregate_count;
        if !entry.semiring.all_distributive {
            result.all_distributive = false;
        }
        result
            .non_distributive_aggregates
            .extend(entry.semiring.non_distributive_aggregates.iter().cloned());
    }
    result
}

fn merge_semiring_from_aggregates(aggregates: Vec<AggregateAnalysis>) -> SemiringAnalysis {
    let mut non_distributive_aggregates = Vec::new();
    let mut all_distributive = true;
    for aggregate in &aggregates {
        if !aggregate.distributive {
            all_distributive = false;
            non_distributive_aggregates.push(aggregate.expression.clone());
        }
    }
    SemiringAnalysis {
        aggregate_count: aggregates.len(),
        all_distributive,
        non_distributive_aggregates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Resolution;

    fn dfc_policy(source: &str, constraint: &str) -> PolicyIr {
        PolicyIr::CompatDfc {
            sources: vec![source.to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: constraint.to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }
    }

    #[test]
    fn indexed_candidates_match_slow_scan() {
        let mut store = PolicyStore::default();
        store.register(dfc_policy("orders", "max(orders.amount) > 1"));
        store.register(dfc_policy("customers", "max(customers.id) > 0"));
        store.register(dfc_policy("products", "max(products.id) > 0"));

        let mut tables = HashSet::new();
        tables.insert(TableKey::new("orders"));

        let indexed = store.candidate_ids_for_tables(&tables);
        store.assert_candidates_match_slow_scan(&tables, None, MultiSourceLookupMode::Subset);
        assert_eq!(indexed, vec![0]);
    }

    #[test]
    fn join_pushdown_index_only_contains_eligible_policies() {
        let mut store = PolicyStore::default();
        store.register(dfc_policy("foo", "max(foo.id) > 1"));
        store.register(PolicyIr::CompatDfc {
            sources: vec!["foo".to_string(), "bar".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
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
        store.register(PolicyIr::CompatDfc {
            sources: vec!["receipts".to_string()],
            required_sources: vec!["receipts".to_string()],
            dimensions: Vec::new(),
            sink: Some("reports".to_string()),
            sink_alias: None,
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
    fn aggregate_policy_index_tracks_compat_aggregate_only() {
        let mut store = PolicyStore::default();
        store.register(dfc_policy("orders", "max(orders.amount) > 1"));
        store.register(PolicyIr::CompatAggregate(
            crate::policy::AggregateDfcPolicy {
                sources: vec!["orders".to_string()],
                dimensions: Vec::new(),
                sink: None,
                constraint: "max(orders.amount) > 1".to_string(),
                description: None,
            },
        ));

        assert_eq!(store.aggregate_policy_indices(), vec![1]);
    }

    #[test]
    fn remove_policy_count_tracks_active_remove_policies() {
        let mut store = PolicyStore::default();
        assert!(!store.has_any_remove_policies());
        let remove_index = store.register(dfc_policy("foo", "foo.id > 1"));
        assert!(store.has_any_remove_policies());
        store.register(PolicyIr::CompatDfc {
            sources: vec!["bar".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
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
            store.register(dfc_policy(&format!("source_{index}"), "id > 0"));
        }
        store.register(PolicyIr::CompatDfc {
            sources: vec!["shared".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "shared.id > 0".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        });
        store.register(PolicyIr::CompatDfc {
            sources: vec!["shared".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
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
        let index = store.register(PolicyIr::CompatDfc {
            sources: vec!["foo".to_string(), "bar".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
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
        let index = store.register(dfc_policy("foo", "foo.id > 1"));
        assert_eq!(store.active_count(), 1);
        assert!(store.deactivate(index));
        assert_eq!(store.active_count(), 0);
    }

    #[test]
    fn sink_only_policies_are_excluded_from_select_scope_without_sink() {
        let mut store = PolicyStore::default();
        store.register(dfc_policy("orders", "max(orders.amount) > 1"));
        store.register(PolicyIr::CompatDfc {
            sources: vec![],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: Some("reports".to_string()),
            sink_alias: None,
            constraint: "max(reports.amount) <= 0".to_string(),
            on_fail: Resolution::Invalidate,
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
        store.register(dfc_policy("foo", "foo.id > 1"));
        store.register(PolicyIr::CompatDfc {
            sources: vec!["foo".to_string(), "bar".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
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
        store.register(PolicyIr::CompatDfc {
            sources: vec!["foo".to_string(), "bar".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
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
    fn aggregate_multi_source_policies_use_subset_indexing() {
        let mut store = PolicyStore::default();
        store.register(PolicyIr::CompatAggregate(
            crate::policy::AggregateDfcPolicy {
                sources: vec!["foo".to_string(), "bar".to_string()],
                dimensions: Vec::new(),
                sink: Some("reports".to_string()),
                constraint: "max(foo.id) + max(bar.id) > 1".to_string(),
                description: None,
            },
        ));
        store.register(PolicyIr::CompatAggregate(
            crate::policy::AggregateDfcPolicy {
                sources: vec!["foo".to_string()],
                dimensions: Vec::new(),
                sink: Some("reports".to_string()),
                constraint: "max(foo.id) > 0".to_string(),
                description: None,
            },
        ));

        let foo_only = HashSet::from([TableKey::new("foo")]);
        let both = HashSet::from([TableKey::new("foo"), TableKey::new("bar")]);
        assert_eq!(
            store.aggregate_policy_indices_for_scope("reports", &foo_only),
            vec![1]
        );
        assert_eq!(
            store.aggregate_policy_indices_for_scope("reports", &both),
            vec![0, 1]
        );
    }

    #[test]
    fn dfc_and_pgn_policy_indices_track_policy_kinds() {
        let mut store = PolicyStore::default();
        store.register(dfc_policy("foo", "foo.id > 1"));
        store.register(PolicyIr::NativePgn(crate::policy::PgnPolicy {
            scope: crate::policy::PolicyScope {
                sources: vec!["foo".to_string()],
                sink: None,
                sink_alias: None,
                dimensions: Vec::new(),
            },
            kind: crate::policy::PgnPolicyKind::Over,
            aggregations: Vec::new(),
            constraint: "foo.id > 0".to_string(),
            on_fail: Resolution::Remove,
            description: None,
            source_text: None,
        }));

        assert_eq!(store.dfc_policy_indices(), vec![0]);
        assert_eq!(store.pgn_policy_indices(), vec![1]);
    }

    #[test]
    fn delete_lookup_uses_source_and_sink_indexes() {
        let mut store = PolicyStore::default();
        store.register(PolicyIr::CompatDfc {
            sources: vec!["orders".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: Some("reports".to_string()),
            sink_alias: None,
            constraint: "reports.id > 0".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        });
        store.register(dfc_policy("other", "other.id > 1"));

        let sink = TableKey::new("reports");
        let ids = store.candidate_ids_for_delete_lookup(Some(&["orders".to_string()]), Some(&sink));
        assert_eq!(ids, vec![0]);
    }

    #[test]
    fn indexed_scope_lookup_matches_slow_scan_for_sink_and_sources() {
        let mut store = PolicyStore::default();
        store.register(dfc_policy("foo", "max(foo.id) > 1"));
        store.register(dfc_policy("bar", "max(bar.id) > 10"));
        store.register(dfc_policy("other", "max(other.id) > 0"));
        store.register(PolicyIr::CompatDfc {
            sources: vec!["receipts".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: Some("reports".to_string()),
            sink_alias: None,
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
        let first = store.register(dfc_policy("orders", "max(orders.amount) > 1"));
        let second = store.register(dfc_policy("orders", "max(orders.amount) > 2"));
        let first_key = store.compiled(first).expect("policy").source_keys[0].clone();
        let second_key = store.compiled(second).expect("policy").source_keys[0].clone();
        assert!(first_key.same_allocation_as(&second_key));
    }

    #[test]
    fn enforcement_index_only_tracks_remove_kill_and_llm_policies() {
        let mut store = PolicyStore::default();
        store.register(dfc_policy("foo", "max(foo.id) > 1"));
        store.register(PolicyIr::CompatDfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(foo.id) > 0".to_string(),
            on_fail: Resolution::Invalidate,
            description: None,
        });
        store.register(PolicyIr::CompatDfc {
            sources: vec!["bar".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
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
            store.register(dfc_policy(
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
        let first = store.register(dfc_policy("orders", "max(orders.amount) > 1"));
        let second = store.register(dfc_policy("customers", "max(orders.amount) > 1"));
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
            store.register(dfc_policy(
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
    fn identical_columns_share_interned_keys() {
        let mut store = PolicyStore::default();
        let first = store.register(dfc_policy("orders", "max(orders.amount) > 1"));
        let second = store.register(dfc_policy("customers", "max(customers.amount) > 1"));
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
