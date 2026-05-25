use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// JSON export of rewrite counters and phase timings for explain output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RewriteStatsExport {
    pub total_policies: usize,
    pub candidate_policies: usize,
    pub applicable_policies: usize,
    pub dominated_policies: usize,
    pub query_nodes: usize,
    pub select_scopes_analyzed: usize,
    pub ast_nodes_visited_analysis: usize,
    pub ast_nodes_visited_rewrite: usize,
    pub policy_constraints_parsed_during_rewrite: usize,
    pub timings: RewriteStatsTimings,
}

impl From<RewriteStats> for RewriteStatsExport {
    fn from(stats: RewriteStats) -> Self {
        Self {
            total_policies: stats.total_policies,
            candidate_policies: stats.candidate_policies,
            applicable_policies: stats.applicable_policies,
            dominated_policies: stats.dominated_policies,
            query_nodes: stats.query_nodes,
            select_scopes_analyzed: stats.select_scopes_analyzed,
            ast_nodes_visited_analysis: stats.ast_nodes_visited_analysis,
            ast_nodes_visited_rewrite: stats.ast_nodes_visited_rewrite,
            policy_constraints_parsed_during_rewrite: stats
                .policy_constraints_parsed_during_rewrite,
            timings: stats.timings(),
        }
    }
}

/// Optional rewrite instrumentation counters (see `passant-perf.md`).
#[derive(Debug, Default, Clone)]
pub struct RewriteStats {
    pub total_policies: usize,
    pub candidate_policies: usize,
    pub applicable_policies: usize,
    pub dominated_policies: usize,
    /// SELECT/CTE/subquery scopes discovered during query analysis.
    pub query_nodes: usize,
    pub select_scopes_analyzed: usize,
    pub ast_nodes_visited_analysis: usize,
    pub ast_nodes_visited_rewrite: usize,
    pub policy_constraints_parsed_during_rewrite: usize,
    pub elapsed_parse: Duration,
    pub elapsed_analysis: Duration,
    pub elapsed_candidate_lookup: Duration,
    pub elapsed_planning: Duration,
    pub elapsed_rewrite: Duration,
    pub elapsed_format: Duration,
}

/// JSON-friendly timing breakdown for explain output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RewriteStatsTimings {
    pub elapsed_parse_ms: f64,
    pub elapsed_analysis_ms: f64,
    pub elapsed_candidate_lookup_ms: f64,
    pub elapsed_planning_ms: f64,
    pub elapsed_rewrite_ms: f64,
    pub elapsed_format_ms: f64,
    pub elapsed_total_ms: f64,
}

impl RewriteStats {
    pub fn timings(&self) -> RewriteStatsTimings {
        RewriteStatsTimings {
            elapsed_parse_ms: duration_to_ms(self.elapsed_parse),
            elapsed_analysis_ms: duration_to_ms(self.elapsed_analysis),
            elapsed_candidate_lookup_ms: duration_to_ms(self.elapsed_candidate_lookup),
            elapsed_planning_ms: duration_to_ms(self.elapsed_planning),
            elapsed_rewrite_ms: duration_to_ms(self.elapsed_rewrite),
            elapsed_format_ms: duration_to_ms(self.elapsed_format),
            elapsed_total_ms: duration_to_ms(
                self.elapsed_parse
                    + self.elapsed_analysis
                    + self.elapsed_candidate_lookup
                    + self.elapsed_planning
                    + self.elapsed_rewrite
                    + self.elapsed_format,
            ),
        }
    }
}

fn duration_to_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[derive(Debug, Default)]
pub(crate) struct RewriteStatsCell {
    inner: Mutex<RewriteStats>,
}

impl RewriteStatsCell {
    fn with_stats<R>(&self, update: impl FnOnce(&mut RewriteStats) -> R) -> R {
        let mut stats = self.inner.lock().expect("rewrite stats lock");
        update(&mut stats)
    }

    pub fn reset(&self, total_policies: usize) {
        self.with_stats(|stats| {
            *stats = RewriteStats {
                total_policies,
                ..RewriteStats::default()
            };
        });
    }

    pub fn snapshot(&self) -> RewriteStats {
        self.inner.lock().expect("rewrite stats lock").clone()
    }

    pub fn accumulate_scope_diagnostics(
        &self,
        candidate_policies: usize,
        applicable_policies: usize,
        dominated_policies: usize,
    ) {
        self.with_stats(|stats| {
            stats.candidate_policies += candidate_policies;
            stats.applicable_policies += applicable_policies;
            stats.dominated_policies += dominated_policies;
        });
    }

    pub fn set_query_nodes(&self, query_nodes: usize) {
        self.with_stats(|stats| stats.query_nodes = query_nodes);
    }

    pub fn record_select_scope(&self) {
        self.with_stats(|stats| stats.select_scopes_analyzed += 1);
    }

    pub fn add_ast_nodes_visited_analysis(&self, count: usize) {
        if count > 0 {
            self.with_stats(|stats| stats.ast_nodes_visited_analysis += count);
        }
    }

    pub fn add_ast_nodes_visited_rewrite(&self, count: usize) {
        if count > 0 {
            self.with_stats(|stats| stats.ast_nodes_visited_rewrite += count);
        }
    }

    pub fn record_constraint_parse(&self) {
        self.with_stats(|stats| stats.policy_constraints_parsed_during_rewrite += 1);
    }

    pub fn add_elapsed_parse(&self, elapsed: Duration) {
        self.with_stats(|stats| stats.elapsed_parse += elapsed);
    }

    pub fn add_elapsed_analysis(&self, elapsed: Duration) {
        self.with_stats(|stats| stats.elapsed_analysis += elapsed);
    }

    pub fn add_elapsed_candidate_lookup(&self, elapsed: Duration) {
        self.with_stats(|stats| stats.elapsed_candidate_lookup += elapsed);
    }

    pub fn add_elapsed_planning(&self, elapsed: Duration) {
        self.with_stats(|stats| stats.elapsed_planning += elapsed);
    }

    pub fn add_elapsed_rewrite(&self, elapsed: Duration) {
        self.with_stats(|stats| stats.elapsed_rewrite += elapsed);
    }

    pub fn add_elapsed_format(&self, elapsed: Duration) {
        self.with_stats(|stats| stats.elapsed_format += elapsed);
    }
}
