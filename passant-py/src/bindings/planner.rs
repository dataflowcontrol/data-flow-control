use passant_core::{PassantPlanner, PassantRewriter, PolicyIr, parse_query, parse_query_to_ir};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use serde::Deserialize;

use super::errors::map_rewrite_error;
use super::policy::parse_resolution;
use super::stats::{PyRewriteStats, PyStatementRewriteSummary};

#[pyclass(module = "passant._passant")]
pub struct PyPlanner {
    pub rewriter: PassantRewriter,
}

#[derive(Debug, Deserialize)]
pub struct PolicySpec {
    pub sources: Vec<String>,
    #[serde(default)]
    pub required_sources: Vec<String>,
    #[serde(default)]
    pub dimensions: Vec<String>,
    pub sink: Option<String>,
    pub sink_alias: Option<String>,
    pub constraint: String,
    pub on_fail: String,
    pub description: Option<String>,
}

#[pymethods]
impl PyPlanner {
    #[new]
    fn new() -> Self {
        Self {
            rewriter: PassantRewriter::new(),
        }
    }

    fn transform_query(&self, query: String) -> PyResult<String> {
        PassantRewriter::new()
            .rewrite(&query)
            .map_err(map_rewrite_error)
    }

    fn transform_query_with_policies(
        &self,
        query: String,
        policies_json: String,
    ) -> PyResult<String> {
        let specs = serde_json::from_str::<Vec<PolicySpec>>(&policies_json)
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let mut rewriter = PassantRewriter::new();
        for spec in specs {
            rewriter
                .register_validated_policy(PolicyIr::Dfc {
                    sources: spec.sources,
                    required_sources: spec.required_sources,
                    dimensions: spec.dimensions,
                    sink: spec.sink,
                    sink_alias: spec.sink_alias,
                    constraint: spec.constraint,
                    on_fail: parse_resolution(&spec.on_fail)?,
                    description: spec.description,
                })
                .map_err(map_rewrite_error)?;
        }
        rewriter.rewrite(&query).map_err(map_rewrite_error)
    }

    fn register_policy_text(&mut self, policy_text: String) -> PyResult<()> {
        self.rewriter
            .register_policy_text(&policy_text)
            .map_err(map_rewrite_error)
    }

    fn register_policy_specs(&mut self, policies_json: String) -> PyResult<()> {
        for policy in parse_policy_specs(&policies_json)? {
            self.rewriter
                .register_validated_policy(policy)
                .map_err(map_rewrite_error)?;
        }
        Ok(())
    }

    fn sync_catalog(&mut self, catalog_json: String) -> PyResult<()> {
        let snapshot = super::catalog::deserialize_catalog_snapshot(&catalog_json)?;
        self.rewriter.apply_catalog_snapshot(snapshot);
        Ok(())
    }

    fn validate_policy_specs(&self, policies_json: String) -> PyResult<()> {
        for policy in parse_policy_specs(&policies_json)? {
            self.rewriter
                .catalog()
                .validate_policy(&policy)
                .map_err(map_rewrite_error)?;
        }
        Ok(())
    }

    #[pyo3(signature = (sources=None, sink=None, constraint=None, on_fail=None, description=None))]
    fn delete_policy(
        &mut self,
        sources: Option<Vec<String>>,
        sink: Option<String>,
        constraint: Option<String>,
        on_fail: Option<String>,
        description: Option<String>,
    ) -> PyResult<bool> {
        let on_fail = on_fail.as_deref().map(parse_resolution).transpose()?;
        Ok(self.rewriter.delete_policy(
            sources.as_deref(),
            sink.as_deref(),
            constraint.as_deref(),
            on_fail,
            description.as_deref(),
        ))
    }

    #[pyo3(signature = (query, use_partial_push=false, collect_stats=false, dialect=None))]
    fn transform_registered(
        &self,
        query: String,
        use_partial_push: bool,
        collect_stats: bool,
        dialect: Option<String>,
    ) -> PyResult<String> {
        let parse_dialect = dialect
            .as_deref()
            .and_then(|value| value.parse::<passant_core::SqlDialect>().ok());
        let options = passant_core::RewriteOptions {
            use_partial_push,
            collect_stats,
            parse_dialect,
        };
        self.rewriter
            .rewrite_with_options(&query, options)
            .map_err(map_rewrite_error)
    }

    fn last_rewrite_stats(&self) -> PyRewriteStats {
        self.rewriter.last_rewrite_stats().into()
    }

    fn last_statement_rewrite_summary(&self) -> PyStatementRewriteSummary {
        self.rewriter.last_statement_rewrite_summary().into()
    }

    fn explain_rewrite_registered(&self, query: String) -> PyResult<String> {
        self.explain_rewrite_registered_with_options(query, false)
    }

    #[pyo3(signature = (query, include_stats=false))]
    fn explain_rewrite_registered_with_options(
        &self,
        query: String,
        include_stats: bool,
    ) -> PyResult<String> {
        let ir = parse_query_to_ir(&query).map_err(|err| PyValueError::new_err(err.to_string()))?;
        let policies = self.rewriter.policies();
        let mut explanation = PassantPlanner::new().explain_rewrite(&ir, &policies);
        let statement =
            parse_query(&query).map_err(|err| PyValueError::new_err(err.to_string()))?;
        let statement_plan = self.rewriter.plan_statement_summary(&statement);
        explanation.policy_plan = Some(statement_plan.aggregate());
        explanation.statement_plan = Some(statement_plan);
        if include_stats {
            let options = passant_core::RewriteOptions {
                collect_stats: true,
                ..passant_core::RewriteOptions::default()
            };
            self.rewriter
                .rewrite_with_options(&query, options)
                .map_err(map_rewrite_error)?;
            explanation.rewrite_stats = Some(self.rewriter.last_rewrite_stats().into());
        }
        serde_json::to_string_pretty(&explanation)
            .map_err(|err| PyValueError::new_err(err.to_string()))
    }

    fn dfc_policies_json(&self) -> PyResult<String> {
        serde_json::to_string_pretty(&self.rewriter.dfc_policies())
            .map_err(|err| PyValueError::new_err(err.to_string()))
    }

    fn has_registered_policies(&self) -> bool {
        self.rewriter.has_registered_policies()
    }

    fn pgn_policies_json(&self) -> PyResult<String> {
        serde_json::to_string_pretty(&self.rewriter.pgn_policies())
            .map_err(|err| PyValueError::new_err(err.to_string()))
    }

    fn explain_rewrite(&self, query: String) -> PyResult<String> {
        let ir = parse_query_to_ir(&query).map_err(|err| PyValueError::new_err(err.to_string()))?;
        let explanation = PassantPlanner::new().explain_rewrite(&ir, &[]);
        serde_json::to_string_pretty(&explanation)
            .map_err(|err| PyValueError::new_err(err.to_string()))
    }

    fn explain_rewrite_with_policies(
        &self,
        query: String,
        policies_json: String,
    ) -> PyResult<String> {
        let specs = serde_json::from_str::<Vec<PolicySpec>>(&policies_json)
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let mut policies = Vec::new();
        for spec in specs {
            policies.push(PolicyIr::Dfc {
                sources: spec.sources,
                required_sources: spec.required_sources,
                dimensions: spec.dimensions,
                sink: spec.sink,
                sink_alias: spec.sink_alias,
                constraint: spec.constraint,
                on_fail: parse_resolution(&spec.on_fail)?,
                description: spec.description,
            });
        }
        let ir = parse_query_to_ir(&query).map_err(|err| PyValueError::new_err(err.to_string()))?;
        let explanation = PassantPlanner::new().explain_rewrite(&ir, &policies);
        serde_json::to_string_pretty(&explanation)
            .map_err(|err| PyValueError::new_err(err.to_string()))
    }

    #[pyo3(signature = (query, sources, constraint, sink=None, on_fail="REMOVE".to_string()))]
    fn plan_with_policy(
        &self,
        query: String,
        sources: Vec<String>,
        constraint: String,
        sink: Option<String>,
        on_fail: String,
    ) -> PyResult<String> {
        let ir = parse_query_to_ir(&query).map_err(|err| PyValueError::new_err(err.to_string()))?;
        let policy = PolicyIr::Dfc {
            sources,
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink,
            sink_alias: None,
            constraint,
            on_fail: parse_resolution(&on_fail)?,
            description: None,
        };
        let result = PassantPlanner::new().plan_query(&ir, &[policy]);
        serde_json::to_string_pretty(&result).map_err(|err| PyValueError::new_err(err.to_string()))
    }
}

#[pyfunction]
pub fn parse_sql_to_ir(query: String) -> PyResult<String> {
    let ir = parse_query_to_ir(&query).map_err(|err| PyValueError::new_err(err.to_string()))?;
    serde_json::to_string_pretty(&ir).map_err(|err| PyValueError::new_err(err.to_string()))
}

pub fn parse_policy_specs(policies_json: &str) -> PyResult<Vec<PolicyIr>> {
    let specs = serde_json::from_str::<Vec<PolicySpec>>(policies_json)
        .map_err(|err| PyValueError::new_err(err.to_string()))?;
    let mut policies = Vec::new();
    for spec in specs {
        policies.push(PolicyIr::Dfc {
            sources: spec.sources,
            required_sources: spec.required_sources,
            dimensions: spec.dimensions,
            sink: spec.sink,
            sink_alias: spec.sink_alias,
            constraint: spec.constraint,
            on_fail: parse_resolution(&spec.on_fail)?,
            description: spec.description,
        });
    }
    Ok(policies)
}
