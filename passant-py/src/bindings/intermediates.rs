use passant_core::intermediate_compile::bind_intermediate_sink;
// Thin DuckDB execution transport for Rust-owned intermediate semantics.
use std::collections::BTreeMap;

use passant_core::intermediate_state::{
    IntermediatePlan, IntermediateSpec, IntermediateState, SqlExecutor, SqlValue,
    validate_transfer_type,
};
use pyo3::create_exception;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};

create_exception!(_passant, IntermediateError, PyValueError);
pub(crate) fn state_error(error: String) -> PyErr {
    IntermediateError::new_err(error)
}

pub(crate) struct PySqlExecutor<'py> {
    pub(crate) conn: Bound<'py, PyAny>,
}
impl<'py> PySqlExecutor<'py> {
    pub(crate) fn new(conn: &Bound<'py, PyAny>) -> Self {
        Self { conn: conn.clone() }
    }
}
impl SqlExecutor for PySqlExecutor<'_> {
    fn query_json(&mut self, sql: &str) -> Result<String, String> {
        self.conn
            .call_method1("execute", (sql,))
            .and_then(|cursor| cursor.call_method0("fetchone"))
            .and_then(|row| row.get_item(0))
            .and_then(|value| value.extract())
            .map_err(|e| e.to_string())
    }
    fn describe_types(&mut self, sql: &str) -> Result<Vec<String>, String> {
        let rows = self
            .conn
            .call_method1("execute", (format!("DESCRIBE ({sql})"),))
            .and_then(|cursor| cursor.call_method0("fetchall"))
            .map_err(|e| e.to_string())?;
        rows.try_iter()
            .map_err(|e| e.to_string())?
            .map(|row| {
                row.and_then(|r| r.get_item(1))
                    .and_then(|v| v.extract())
                    .map_err(|e| e.to_string())
            })
            .collect()
    }
}

pub(crate) fn typed_sink(
    conn: &Bound<'_, PyAny>,
    sink: Option<&Bound<'_, PyDict>>,
) -> PyResult<BTreeMap<String, SqlValue>> {
    let mut result = BTreeMap::new();
    if let Some(sink) = sink {
        for (key, value) in sink {
            let key: String = key.extract()?;
            let params = PyList::new(conn.py(), [value])?;
            let sql = "SELECT json_object('sql_type',typeof(v),'value',CAST(v AS VARCHAR)) FROM (SELECT ? AS v)";
            let encoded: String = conn
                .call_method1("execute", (sql, params))?
                .call_method0("fetchone")?
                .get_item(0)?
                .extract()?;
            result.insert(
                key,
                serde_json::from_str(&encoded).map_err(|e| state_error(e.to_string()))?,
            );
        }
    }
    Ok(result)
}

#[pyclass(module = "data_flow_control._passant", frozen)]
#[derive(Clone)]
pub struct PyIntermediateState {
    pub(crate) inner: IntermediateState,
}

#[pyclass(module = "data_flow_control._passant")]
pub struct PyIntermediatePlan {
    pub(crate) inner: IntermediatePlan,
}
#[pymethods]
impl PyIntermediatePlan {
    #[new]
    #[pyo3(signature=(specs_json,sources,required_sources,sink_endpoint=true))]
    fn new(
        specs_json: &str,
        sources: Vec<String>,
        required_sources: Vec<String>,
        sink_endpoint: bool,
    ) -> PyResult<Self> {
        let specs: Vec<IntermediateSpec> =
            serde_json::from_str(specs_json).map_err(|e| state_error(e.to_string()))?;
        Ok(Self {
            inner: IntermediatePlan::new(specs, sources, required_sources)
                .map_err(state_error)?
                .with_sink_endpoint(sink_endpoint),
        })
    }
    fn empty(&self) -> PyIntermediateState {
        PyIntermediateState {
            inner: self.inner.empty(),
        }
    }
    fn capture(
        &self,
        connection: &Bound<'_, PyAny>,
        source: &str,
        query: &str,
        identity_columns: Vec<String>,
    ) -> PyResult<PyIntermediateState> {
        Ok(PyIntermediateState {
            inner: self
                .inner
                .capture(
                    &mut PySqlExecutor::new(connection),
                    source,
                    query,
                    &identity_columns,
                )
                .map_err(state_error)?,
        })
    }
    fn merge(
        &self,
        connection: &Bound<'_, PyAny>,
        states: Vec<PyRef<'_, PyIntermediateState>>,
    ) -> PyResult<PyIntermediateState> {
        let states: Vec<_> = states.iter().map(|s| &s.inner).collect();
        Ok(PyIntermediateState {
            inner: self
                .inner
                .merge(&mut PySqlExecutor::new(connection), &states)
                .map_err(state_error)?,
        })
    }
    fn applicable(&self, state: &PyIntermediateState) -> PyResult<bool> {
        self.inner.applicable(&state.inner).map_err(state_error)
    }
    fn validate(&self, state: &PyIntermediateState) -> PyResult<()> {
        self.inner.validate(&state.inner).map_err(state_error)
    }
    fn values<'py>(
        &self,
        connection: &Bound<'py, PyAny>,
        state: &PyIntermediateState,
    ) -> PyResult<Bound<'py, PyDict>> {
        let fields = self
            .inner
            .values(&mut PySqlExecutor::new(connection), &state.inner)
            .map_err(state_error)?;
        let result = PyDict::new(connection.py());
        for (name, values) in fields {
            let sql = format!(
                "SELECT {}",
                values
                    .iter()
                    .map(SqlValue::sql)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let row = connection
                .call_method1("execute", (sql,))?
                .call_method0("fetchone")?;
            if values.len() == 2 {
                result.set_item(name, row.downcast::<PyTuple>()?)?;
            } else {
                result.set_item(name, row.get_item(0)?)?;
            }
        }
        Ok(result)
    }
    #[pyo3(signature=(connection,state,predicate,sink=None))]
    fn evaluate(
        &self,
        connection: &Bound<'_, PyAny>,
        state: &PyIntermediateState,
        predicate: &str,
        sink: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Option<bool>> {
        let sink = typed_sink(connection, sink)?;
        self.inner
            .evaluate(
                &mut PySqlExecutor::new(connection),
                &state.inner,
                predicate,
                sink,
            )
            .map_err(state_error)
    }
    #[pyo3(signature=(connection,state,predicate,sink_columns,sink=None))]
    fn evaluate_bound(
        &self,
        connection: &Bound<'_, PyAny>,
        state: &PyIntermediateState,
        predicate: &str,
        sink_columns: Vec<(String, String)>,
        sink: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Option<bool>> {
        let sink = bind_intermediate_sink(&sink_columns, &typed_sink(connection, sink)?)
            .map_err(state_error)?;
        self.inner
            .evaluate(
                &mut PySqlExecutor::new(connection),
                &state.inner,
                predicate,
                sink,
            )
            .map_err(state_error)
    }
}

#[pyfunction]
pub fn validate_intermediate_spec_json(spec_json: &str) -> PyResult<()> {
    let spec: IntermediateSpec =
        serde_json::from_str(spec_json).map_err(|e| state_error(e.to_string()))?;
    spec.validate().map_err(state_error)
}
#[pyfunction]
#[pyo3(signature=(sql_type,identity=false))]
pub fn validate_intermediate_transfer_type(sql_type: &str, identity: bool) -> PyResult<()> {
    validate_transfer_type(sql_type, identity).map_err(state_error)
}
