//! DB-API and Python-object marshalling for the Rust session engine.
use std::collections::HashMap;

use passant_core::intermediate_session::{IntermediateDependency, IntermediateSession};
use passant_core::intermediate_state::IntermediateState;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyTuple;

use super::intermediates::{PyIntermediatePlan, PyIntermediateState, PySqlExecutor};

#[pyclass(name = "PyIntermediateSession")]
pub struct PyIntermediateSession {
    inner: IntermediateSession,
}

fn states(rows: &Bound<'_, PyAny>) -> PyResult<Vec<Vec<IntermediateState>>> {
    rows.try_iter()?
        .map(|row| {
            row?.try_iter()?
                .map(|state| {
                    Ok(state?
                        .extract::<PyRef<'_, PyIntermediateState>>()?
                        .inner
                        .clone())
                })
                .collect()
        })
        .collect()
}

fn state_tuple(py: Python<'_>, states: Vec<IntermediateState>) -> PyResult<Py<PyTuple>> {
    let objects: PyResult<Vec<_>> = states
        .into_iter()
        .map(|inner| Py::new(py, PyIntermediateState { inner }))
        .collect();
    Ok(PyTuple::new(py, objects?)?.unbind())
}

#[pymethods]
impl PyIntermediateSession {
    #[new]
    fn new(plans: &Bound<'_, PyAny>, sources: HashMap<String, String>) -> PyResult<Self> {
        let plans: PyResult<Vec<_>> = plans
            .try_iter()?
            .map(|plan| {
                Ok(plan?
                    .extract::<PyRef<'_, PyIntermediatePlan>>()?
                    .inner
                    .clone())
            })
            .collect();
        Ok(Self {
            inner: IntermediateSession::new(plans?, sources).map_err(PyValueError::new_err)?,
        })
    }

    fn row_states(
        &self,
        py: Python<'_>,
        conn: &Bound<'_, PyAny>,
        dependencies: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyTuple>> {
        let mut parsed = Vec::new();
        if !dependencies.is_none() {
            for dependency in dependencies.try_iter()? {
                let dependency = dependency?;
                parsed.push(IntermediateDependency {
                    t: dependency.get_item("t")?.extract()?,
                    r: dependency.get_item("r")?.extract()?,
                });
            }
        }
        let result = self
            .inner
            .row_states(&mut PySqlExecutor { conn: conn.clone() }, &parsed)
            .map_err(PyValueError::new_err)?;
        state_tuple(py, result)
    }

    fn merge_rows(
        &self,
        py: Python<'_>,
        conn: &Bound<'_, PyAny>,
        rows: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyTuple>> {
        let result = self
            .inner
            .merge_rows(&mut PySqlExecutor { conn: conn.clone() }, &states(rows)?)
            .map_err(PyValueError::new_err)?;
        state_tuple(py, result)
    }

    fn register_rows(&mut self, rows: &Bound<'_, PyAny>) -> PyResult<Vec<i64>> {
        self.inner
            .register_rows(states(rows)?)
            .map_err(PyValueError::new_err)
    }

    fn checkpoint(&self) -> i64 {
        self.inner.checkpoint()
    }

    fn rollback(&mut self, checkpoint: i64) -> PyResult<()> {
        self.inner
            .rollback(checkpoint)
            .map_err(PyValueError::new_err)
    }
}
