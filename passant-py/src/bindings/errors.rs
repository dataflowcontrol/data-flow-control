use passant_core::RewriteError;
use pyo3::create_exception;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

create_exception!(
    _passant,
    PassantRewriteError,
    pyo3::exceptions::PyValueError
);

pub fn map_rewrite_error(err: RewriteError) -> PyErr {
    Python::with_gil(|py| -> PyResult<PyErr> {
        let py_err = PassantRewriteError::new_err(err.to_string());
        py_err.value(py).setattr("kind", err.kind().as_str())?;
        Ok(py_err)
    })
    .unwrap_or_else(|e| e)
}

pub fn map_policy_parse_error(err: passant_core::PolicyParseError) -> PyErr {
    PyValueError::new_err(err.to_string())
}
