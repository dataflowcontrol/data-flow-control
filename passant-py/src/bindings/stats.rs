use passant_core::{RewriteStats, RewriteStatsTimings, StatementRewriteSummary};
use pyo3::prelude::*;

#[pyclass(module = "data_flow_control._passant")]
#[derive(Clone, Copy)]
pub struct PyRewriteStatsTimings {
    #[pyo3(get)]
    pub elapsed_parse_ms: f64,
    #[pyo3(get)]
    pub elapsed_analysis_ms: f64,
    #[pyo3(get)]
    pub elapsed_statement_tables_ms: f64,
    #[pyo3(get)]
    pub elapsed_scope_analysis_ms: f64,
    #[pyo3(get)]
    pub elapsed_candidate_lookup_ms: f64,
    #[pyo3(get)]
    pub elapsed_planning_ms: f64,
    #[pyo3(get)]
    pub elapsed_rewrite_ms: f64,
    #[pyo3(get)]
    pub elapsed_format_ms: f64,
    #[pyo3(get)]
    pub elapsed_total_ms: f64,
}

impl From<RewriteStatsTimings> for PyRewriteStatsTimings {
    fn from(timings: RewriteStatsTimings) -> Self {
        Self {
            elapsed_parse_ms: timings.elapsed_parse_ms,
            elapsed_analysis_ms: timings.elapsed_analysis_ms,
            elapsed_statement_tables_ms: timings.elapsed_statement_tables_ms,
            elapsed_scope_analysis_ms: timings.elapsed_scope_analysis_ms,
            elapsed_candidate_lookup_ms: timings.elapsed_candidate_lookup_ms,
            elapsed_planning_ms: timings.elapsed_planning_ms,
            elapsed_rewrite_ms: timings.elapsed_rewrite_ms,
            elapsed_format_ms: timings.elapsed_format_ms,
            elapsed_total_ms: timings.elapsed_total_ms,
        }
    }
}

#[pyclass(module = "data_flow_control._passant")]
#[derive(Clone)]
pub struct PyRewriteStats {
    #[pyo3(get)]
    pub total_policies: usize,
    #[pyo3(get)]
    pub candidate_policies: usize,
    #[pyo3(get)]
    pub applicable_policies: usize,
    #[pyo3(get)]
    pub dominated_policies: usize,
    #[pyo3(get)]
    pub query_nodes: usize,
    #[pyo3(get)]
    pub select_scopes_analyzed: usize,
    #[pyo3(get)]
    pub ast_nodes_visited_analysis: usize,
    #[pyo3(get)]
    pub ast_nodes_visited_rewrite: usize,
    #[pyo3(get)]
    pub policy_constraints_parsed_during_rewrite: usize,
    #[pyo3(get)]
    pub timings: PyRewriteStatsTimings,
}

impl From<RewriteStats> for PyRewriteStats {
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
            timings: stats.timings().into(),
        }
    }
}

#[pyclass(module = "data_flow_control._passant")]
#[derive(Clone)]
pub struct PyStatementRewriteSummary {
    #[pyo3(get)]
    pub scope_count: usize,
    #[pyo3(get)]
    pub candidate_policies: usize,
    #[pyo3(get)]
    pub applicable_policies: usize,
    #[pyo3(get)]
    pub dominated_policies: usize,
    #[pyo3(get)]
    pub warnings: Vec<String>,
}

impl From<StatementRewriteSummary> for PyStatementRewriteSummary {
    fn from(summary: StatementRewriteSummary) -> Self {
        let aggregate = summary.aggregate();
        Self {
            scope_count: summary.scope_diagnostics.len(),
            candidate_policies: aggregate.candidate_policies,
            applicable_policies: aggregate.applicable_policies,
            dominated_policies: aggregate.dominated_policies,
            warnings: aggregate.warnings,
        }
    }
}
