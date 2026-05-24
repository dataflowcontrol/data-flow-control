use passant_core::{
    CatalogSnapshot, PassantPlanner, PassantRewriter, PolicyIr, Resolution, RewriteError,
    normalize_policy_dimensions, normalize_policy_sources, parse_policy_text, parse_query_to_ir,
    validate_constraint_expression,
};
use pyo3::create_exception;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use serde::Deserialize;

create_exception!(
    _passant,
    PassantRewriteError,
    pyo3::exceptions::PyValueError
);

#[pyclass(module = "passant._passant")]
#[derive(Clone)]
struct PyDfcPolicy {
    #[pyo3(get)]
    sources: Vec<String>,
    #[pyo3(get)]
    sink: Option<String>,
    #[pyo3(get)]
    sink_alias: Option<String>,
    #[pyo3(get)]
    constraint: String,
    #[pyo3(get)]
    on_fail: String,
    #[pyo3(get)]
    description: Option<String>,
}

#[pymethods]
impl PyDfcPolicy {
    #[new]
    #[pyo3(signature = (constraint, sources, on_fail="REMOVE".to_string(), sink=None, sink_alias=None, description=None))]
    fn new(
        constraint: String,
        sources: Vec<String>,
        on_fail: String,
        sink: Option<String>,
        sink_alias: Option<String>,
        description: Option<String>,
    ) -> PyResult<Self> {
        let _ = parse_resolution(&on_fail)?;
        Ok(Self {
            sources,
            sink,
            sink_alias,
            constraint,
            on_fail,
            description,
        })
    }
}

#[pyclass(module = "passant._passant")]
struct PyPlanner {
    rewriter: PassantRewriter,
}

#[derive(Debug, Deserialize)]
struct PolicySpec {
    sources: Vec<String>,
    #[serde(default)]
    required_sources: Vec<String>,
    #[serde(default)]
    dimensions: Vec<String>,
    sink: Option<String>,
    sink_alias: Option<String>,
    constraint: String,
    on_fail: String,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AggregatePolicySpec {
    sources: Vec<String>,
    #[serde(default)]
    dimensions: Vec<String>,
    sink: Option<String>,
    constraint: String,
    description: Option<String>,
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
                .register_validated_policy(PolicyIr::CompatDfc {
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

    fn transform_query_with_all_policies(
        &self,
        query: String,
        policies_json: String,
        aggregate_policies_json: String,
    ) -> PyResult<String> {
        let specs = serde_json::from_str::<Vec<PolicySpec>>(&policies_json)
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let aggregate_specs =
            serde_json::from_str::<Vec<AggregatePolicySpec>>(&aggregate_policies_json)
                .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let mut rewriter = PassantRewriter::new();
        for spec in specs {
            rewriter
                .register_validated_policy(PolicyIr::CompatDfc {
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
        for spec in aggregate_specs {
            rewriter
                .register_validated_policy(PolicyIr::CompatAggregate(
                    passant_core::AggregateDfcPolicy {
                        sources: spec.sources,
                        dimensions: spec.dimensions,
                        sink: spec.sink,
                        constraint: spec.constraint,
                        description: spec.description,
                    },
                ))
                .map_err(map_rewrite_error)?;
        }
        rewriter.rewrite(&query).map_err(map_rewrite_error)
    }

    fn aggregate_finalization_queries(
        &self,
        sink_table: String,
        policies_json: String,
    ) -> PyResult<String> {
        let specs = serde_json::from_str::<Vec<AggregatePolicySpec>>(&policies_json)
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let mut rewriter = PassantRewriter::new();
        for spec in specs {
            rewriter.register_policy(PolicyIr::CompatAggregate(
                passant_core::AggregateDfcPolicy {
                    sources: spec.sources,
                    dimensions: spec.dimensions,
                    sink: spec.sink,
                    constraint: spec.constraint,
                    description: spec.description,
                },
            ));
        }
        finalization_queries_json(&rewriter, &sink_table)
    }

    fn register_policy_text(&mut self, policy_text: String) -> PyResult<()> {
        self.rewriter
            .register_policy_text(&policy_text)
            .map_err(map_rewrite_error)
    }

    fn register_policy_specs(
        &mut self,
        policies_json: String,
        aggregate_policies_json: String,
    ) -> PyResult<()> {
        for policy in parse_policy_specs(&policies_json, &aggregate_policies_json)? {
            self.rewriter
                .register_validated_policy(policy)
                .map_err(map_rewrite_error)?;
        }
        Ok(())
    }

    fn sync_catalog(&mut self, catalog_json: String) -> PyResult<()> {
        let snapshot = serde_json::from_str::<CatalogSnapshot>(&catalog_json)
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        self.rewriter.catalog_mut().load_snapshot(snapshot);
        Ok(())
    }

    fn validate_policy_specs(
        &self,
        policies_json: String,
        aggregate_policies_json: String,
    ) -> PyResult<()> {
        for policy in parse_policy_specs(&policies_json, &aggregate_policies_json)? {
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

    #[pyo3(signature = (query, use_partial_push=false))]
    fn transform_registered(&self, query: String, use_partial_push: bool) -> PyResult<String> {
        let options = passant_core::RewriteOptions { use_partial_push };
        self.rewriter
            .rewrite_with_options(&query, options)
            .map_err(map_rewrite_error)
    }

    fn explain_rewrite_registered(&self, query: String) -> PyResult<String> {
        let ir = parse_query_to_ir(&query).map_err(|err| PyValueError::new_err(err.to_string()))?;
        let explanation = PassantPlanner::new().explain_rewrite(&ir, self.rewriter.policies());
        serde_json::to_string_pretty(&explanation)
            .map_err(|err| PyValueError::new_err(err.to_string()))
    }

    fn aggregate_finalization_registered(&self, sink_table: String) -> PyResult<String> {
        finalization_queries_json(&self.rewriter, &sink_table)
    }

    fn dfc_policies_json(&self) -> PyResult<String> {
        serde_json::to_string_pretty(&self.rewriter.dfc_policies())
            .map_err(|err| PyValueError::new_err(err.to_string()))
    }

    fn aggregate_policies_json(&self) -> PyResult<String> {
        serde_json::to_string_pretty(&self.rewriter.aggregate_policies())
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
        aggregate_policies_json: String,
    ) -> PyResult<String> {
        let specs = serde_json::from_str::<Vec<PolicySpec>>(&policies_json)
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let aggregate_specs =
            serde_json::from_str::<Vec<AggregatePolicySpec>>(&aggregate_policies_json)
                .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let mut policies = Vec::new();
        for spec in specs {
            policies.push(PolicyIr::CompatDfc {
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
        for spec in aggregate_specs {
            policies.push(PolicyIr::CompatAggregate(
                passant_core::AggregateDfcPolicy {
                    sources: spec.sources,
                    dimensions: spec.dimensions,
                    sink: spec.sink,
                    constraint: spec.constraint,
                    description: spec.description,
                },
            ));
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
        let policy = PolicyIr::CompatDfc {
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
fn parse_sql_to_ir(query: String) -> PyResult<String> {
    let ir = parse_query_to_ir(&query).map_err(|err| PyValueError::new_err(err.to_string()))?;
    serde_json::to_string_pretty(&ir).map_err(|err| PyValueError::new_err(err.to_string()))
}

fn map_rewrite_error(err: RewriteError) -> PyErr {
    Python::with_gil(|py| -> PyResult<PyErr> {
        let py_err = PassantRewriteError::new_err(err.to_string());
        py_err.value(py).setattr("kind", err.kind().as_str())?;
        Ok(py_err)
    })
    .unwrap_or_else(|e| e)
}

fn parse_policy_specs(
    policies_json: &str,
    aggregate_policies_json: &str,
) -> PyResult<Vec<PolicyIr>> {
    let specs = serde_json::from_str::<Vec<PolicySpec>>(policies_json)
        .map_err(|err| PyValueError::new_err(err.to_string()))?;
    let aggregate_specs = serde_json::from_str::<Vec<AggregatePolicySpec>>(aggregate_policies_json)
        .map_err(|err| PyValueError::new_err(err.to_string()))?;
    let mut policies = Vec::new();
    for spec in specs {
        policies.push(PolicyIr::CompatDfc {
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
    for spec in aggregate_specs {
        policies.push(PolicyIr::CompatAggregate(
            passant_core::AggregateDfcPolicy {
                sources: spec.sources,
                dimensions: spec.dimensions,
                sink: spec.sink,
                constraint: spec.constraint,
                description: spec.description,
            },
        ));
    }
    Ok(policies)
}

fn finalization_queries_json(rewriter: &PassantRewriter, sink_table: &str) -> PyResult<String> {
    let queries = rewriter
        .finalize_aggregate_queries(sink_table)
        .into_iter()
        .map(|query| {
            serde_json::json!({
                "policy_id": query.policy_id,
                "sql": query.sql,
                "invalidate_sql": query.invalidate_sql,
                "description": query.description,
                "constraint": query.constraint,
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string_pretty(&queries).map_err(|err| PyValueError::new_err(err.to_string()))
}

#[pyfunction]
fn parse_policy_to_json(policy_text: String) -> PyResult<String> {
    let policy =
        parse_policy_text(&policy_text).map_err(|err| PyValueError::new_err(err.to_string()))?;
    serde_json::to_string_pretty(&policy).map_err(|err| PyValueError::new_err(err.to_string()))
}

#[pyfunction]
fn validate_constraint_expression_py(sql: String, label: String) -> PyResult<()> {
    validate_constraint_expression(&sql, &label).map_err(map_rewrite_error)
}

#[pyfunction]
fn normalize_policy_sources_py(sources: Vec<String>) -> PyResult<Vec<String>> {
    normalize_policy_sources(&sources).map_err(map_policy_parse_error)
}

#[pyfunction]
fn normalize_policy_dimensions_py(dimensions: Vec<String>) -> PyResult<Vec<String>> {
    normalize_policy_dimensions(&dimensions).map_err(map_policy_parse_error)
}

fn map_policy_parse_error(err: passant_core::PolicyParseError) -> PyErr {
    PyValueError::new_err(err.to_string())
}

#[pymodule]
fn _passant(_py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyDfcPolicy>()?;
    module.add_class::<PyPlanner>()?;
    module.add("PassantRewriteError", _py.get_type::<PassantRewriteError>())?;
    module.add_function(wrap_pyfunction!(parse_sql_to_ir, module)?)?;
    module.add_function(wrap_pyfunction!(parse_policy_to_json, module)?)?;
    module.add_function(wrap_pyfunction!(validate_constraint_expression_py, module)?)?;
    module.add_function(wrap_pyfunction!(normalize_policy_sources_py, module)?)?;
    module.add_function(wrap_pyfunction!(normalize_policy_dimensions_py, module)?)?;
    Ok(())
}

fn parse_resolution(value: &str) -> PyResult<Resolution> {
    match value.to_ascii_uppercase().as_str() {
        "REMOVE" => Ok(Resolution::Remove),
        "KILL" => Ok(Resolution::Kill),
        "INVALIDATE" => Ok(Resolution::Invalidate),
        "INVALIDATE_MESSAGE" => Ok(Resolution::InvalidateMessage),
        "LLM" | "UDF" => Ok(Resolution::Llm),
        _ => Err(PyValueError::new_err(format!("unknown resolution {value}"))),
    }
}
