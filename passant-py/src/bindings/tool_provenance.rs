//! Host-language conversion only; SQL and correspondence decisions are native.
use passant_core::tool_correspondence::{
    ToolKeyRole, align_tool_keys, tool_key_positions, validate_tool_keys,
    validate_tool_result_alignment,
};
use passant_core::tool_provenance::{ToolProvenanceModel, ToolPublicationPlan};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use super::errors::map_rewrite_error;
use super::planner::PyPlanner;
use super::tool_correspondence::extract_key_rows;

#[pyclass(name = "ToolProvenanceModel", module = "data_flow_control._passant")]
pub struct PyToolProvenanceModel {
    model: ToolProvenanceModel,
}

#[pymethods]
impl PyToolProvenanceModel {
    #[new]
    #[pyo3(signature = (sql, key_columns=Vec::new()))]
    fn new(sql: &str, key_columns: Vec<String>) -> PyResult<Self> {
        Ok(Self {
            model: ToolProvenanceModel::new(sql, key_columns).map_err(map_rewrite_error)?,
        })
    }

    #[getter]
    fn sql(&self) -> String {
        self.model.sql()
    }

    #[getter]
    fn key_columns(&self) -> Vec<String> {
        self.model.key_columns().to_vec()
    }

    fn plan(
        &self,
        planner: &PyPlanner,
        sink: &str,
        actual_keys: &Bound<'_, PyAny>,
        row_columns: Vec<Vec<String>>,
        model_parameter_count: usize,
    ) -> PyResult<PyToolPublicationPlan> {
        let keys = extract_key_rows(actual_keys, None)?;
        validate_tool_keys(&keys, self.model.key_columns().len(), ToolKeyRole::Actual)
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let arities = keys.iter().map(Vec::len).collect::<Vec<_>>();
        let inner = planner
            .rewriter
            .plan_tool_publication(
                &self.model,
                sink,
                &arities,
                &row_columns,
                model_parameter_count,
            )
            .map_err(map_rewrite_error)?;
        Ok(PyToolPublicationPlan { inner })
    }

    fn validate_modeled_keys(
        &self,
        modeled_rows: &Bound<'_, PyAny>,
        actual_keys: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let positions = (0..self.model.key_columns().len()).collect::<Vec<_>>();
        let modeled = extract_key_rows(modeled_rows, Some(&positions))?;
        let actual = extract_key_rows(actual_keys, None)?;
        align_tool_keys(&modeled, &actual, self.model.key_columns().len())
            .map(|_| ())
            .map_err(|err| PyValueError::new_err(err.to_string()))
    }

    #[pyo3(signature = (result_columns, result_rows, state_count, actual_keys))]
    fn bind_indices(
        &self,
        result_columns: Option<Vec<String>>,
        result_rows: &Bound<'_, PyAny>,
        state_count: Option<usize>,
        actual_keys: &Bound<'_, PyAny>,
    ) -> PyResult<Vec<usize>> {
        validate_tool_result_alignment(result_rows.len()?, state_count, result_columns.is_some())
            .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let positions = tool_key_positions(
            self.model.key_columns(),
            result_columns.as_deref().unwrap_or_default(),
        )
        .map_err(|err| PyValueError::new_err(err.to_string()))?;
        let modeled = extract_key_rows(result_rows, Some(&positions))?;
        let actual = extract_key_rows(actual_keys, None)?;
        align_tool_keys(&modeled, &actual, self.model.key_columns().len())
            .map_err(|err| PyValueError::new_err(err.to_string()))
    }
}

#[pyclass(name = "ToolPublicationPlan", module = "data_flow_control._passant")]
pub struct PyToolPublicationPlan {
    inner: ToolPublicationPlan,
}

#[pymethods]
impl PyToolPublicationPlan {
    #[getter]
    fn model_keys_sql(&self) -> &str {
        &self.inner.model_keys_sql
    }

    #[getter]
    fn validation_sql(&self) -> &str {
        &self.inner.validation_sql
    }

    #[getter]
    fn publication_sql(&self) -> Option<&str> {
        self.inner.publication_sql.as_deref()
    }

    #[getter]
    fn sink_kill_preflight_sql(&self) -> Option<&str> {
        self.inner.sink_kill_preflight_sql.as_deref()
    }

    #[getter]
    fn sink_preflight_binding_indices(&self) -> Vec<usize> {
        self.inner.sink_preflight_binding_indices.clone()
    }

    #[getter]
    fn output_columns(&self) -> Vec<String> {
        self.inner.output_columns.clone()
    }

    fn check_validation(&self, status: i64) -> PyResult<()> {
        ToolPublicationPlan::check_validation(status).map_err(map_rewrite_error)
    }
}
