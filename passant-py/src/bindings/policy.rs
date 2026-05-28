use passant_core::{
    Resolution, normalize_policy_dimensions, normalize_policy_sources, parse_policy_text,
    validate_constraint_expression,
};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use super::errors::{map_policy_parse_error, map_rewrite_error};

#[pyclass(module = "passant._passant")]
#[derive(Clone)]
pub struct PyDfcPolicy {
    #[pyo3(get)]
    pub sources: Vec<String>,
    #[pyo3(get)]
    pub sink: Option<String>,
    #[pyo3(get)]
    pub sink_alias: Option<String>,
    #[pyo3(get)]
    pub constraint: String,
    #[pyo3(get)]
    pub on_fail: String,
    #[pyo3(get)]
    pub description: Option<String>,
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

#[pyfunction]
pub fn parse_policy_to_json(policy_text: String) -> PyResult<String> {
    let policy =
        parse_policy_text(&policy_text).map_err(|err| PyValueError::new_err(err.to_string()))?;
    serde_json::to_string_pretty(&policy).map_err(|err| PyValueError::new_err(err.to_string()))
}

#[pyfunction]
pub fn validate_constraint_expression_py(sql: String, label: String) -> PyResult<()> {
    validate_constraint_expression(&sql, &label).map_err(map_rewrite_error)
}

#[pyfunction]
pub fn normalize_policy_sources_py(sources: Vec<String>) -> PyResult<Vec<String>> {
    normalize_policy_sources(&sources).map_err(map_policy_parse_error)
}

#[pyfunction]
pub fn normalize_policy_dimensions_py(dimensions: Vec<String>) -> PyResult<Vec<String>> {
    normalize_policy_dimensions(&dimensions).map_err(map_policy_parse_error)
}

pub fn parse_resolution(value: &str) -> PyResult<Resolution> {
    match value.to_ascii_uppercase().as_str() {
        "REMOVE" => Ok(Resolution::Remove),
        "KILL" => Ok(Resolution::Kill),
        _ => Err(PyValueError::new_err(format!("unknown resolution {value}"))),
    }
}
