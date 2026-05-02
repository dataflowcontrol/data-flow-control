use passant_core::{PassantPlanner, PolicyIr, Resolution, parse_query_to_ir};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

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
            description,
        })
    }
}

#[pyclass(module = "passant._passant")]
struct PyPlanner;

#[pymethods]
impl PyPlanner {
    #[new]
    fn new() -> Self {
        Self
    }

    fn transform_query(&self, query: String) -> PyResult<String> {
        let ir = parse_query_to_ir(&query).map_err(|err| PyValueError::new_err(err.to_string()))?;
        let result = PassantPlanner::new().plan_query(&ir, &[]);
        Ok(result.chosen.rewritten_sql)
    }

    fn explain_rewrite(&self, query: String) -> PyResult<String> {
        let ir = parse_query_to_ir(&query).map_err(|err| PyValueError::new_err(err.to_string()))?;
        let explanation = PassantPlanner::new().explain_rewrite(&ir, &[]);
        serde_json::to_string_pretty(&explanation)
            .map_err(|err| PyValueError::new_err(err.to_string()))
    }

    #[pyo3(signature = (query, sources, constraint, sink=None))]
    fn plan_with_policy(
        &self,
        query: String,
        sources: Vec<String>,
        constraint: String,
        sink: Option<String>,
    ) -> PyResult<String> {
        let ir = parse_query_to_ir(&query).map_err(|err| PyValueError::new_err(err.to_string()))?;
        let policy = PolicyIr::CompatDfc {
            sources,
            sink,
            sink_alias: None,
            constraint,
            on_fail: Resolution::Remove,
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

#[pymodule]
fn _passant(_py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyDfcPolicy>()?;
    module.add_class::<PyPlanner>()?;
    module.add_function(wrap_pyfunction!(parse_sql_to_ir, module)?)?;
    Ok(())
}

fn parse_resolution(value: &str) -> PyResult<Resolution> {
    match value.to_ascii_uppercase().as_str() {
        "REMOVE" => Ok(Resolution::Remove),
        "KILL" => Ok(Resolution::Kill),
        "INVALIDATE" => Ok(Resolution::Invalidate),
        "INVALIDATE_MESSAGE" => Ok(Resolution::InvalidateMessage),
        "LLM" => Ok(Resolution::Llm),
        _ => Err(PyValueError::new_err(format!("unknown resolution {value}"))),
    }
}
