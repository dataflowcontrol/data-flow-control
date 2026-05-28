use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use smallvec::SmallVec;
use sqlparser::ast::Expr;

use super::compiled::{is_enforcement_resolution, merge_semiring};
use super::{CompiledPolicy, MultiSourceLookupMode, PolicyStore};
use crate::diagnostics::RewriteError;
use crate::identifiers::TableKey;
use crate::policy::{PolicyIr, Resolution};
use crate::policy_index::{MergedPolicyIndexView, PolicyIndex};
use crate::semiring::SemiringAnalysis;
use crate::sql::parse_projection_expr;

impl PolicyStore {
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

    pub fn join_pushdown_candidates(&self, source: &TableKey) -> Vec<usize> {
        self.join_pushdown_by_source
            .get(source)
            .map(|ids| ids.collect_active_sorted(self))
            .unwrap_or_default()
    }

    /// Candidate enforcement policies (REMOVE/KILL) reachable from visible tables.
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

    pub(crate) fn index_entry(&mut self, entry: &CompiledPolicy) {
        let index = entry.index;

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
        match &entry.policy {
            PolicyIr::Dfc { .. } => self.dfc_policies.push(index),
            PolicyIr::NativePgn(_) => self.pgn_policies.push(index),
        }
        if entry.policy.resolution() == Resolution::Remove {
            self.remove_policy_count += 1;
        }
    }

    pub(crate) fn active_ids<'a>(&'a self, ids: &'a [usize]) -> impl Iterator<Item = usize> + 'a {
        ids.iter()
            .copied()
            .filter(|&index| self.entries.get(index).is_some_and(|entry| entry.active))
    }
}
