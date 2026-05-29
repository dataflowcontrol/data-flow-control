use passant_core::{
    Resolution, normalize_policy_dimensions, normalize_policy_source_aliases,
    normalize_policy_sources, parse_policy_text, validate_constraint_expression,
};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use super::errors::{map_policy_parse_error, map_rewrite_error};

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
pub fn normalize_policy_source_aliases_py(
    sources: Vec<String>,
) -> PyResult<std::collections::HashMap<String, String>> {
    normalize_policy_source_aliases(&sources).map_err(map_policy_parse_error)
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
