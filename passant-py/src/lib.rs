mod bindings;

use bindings::{
    PassantRewriteError, PyDfcPolicy, PyPlanner, PyRewriteStats, PyRewriteStatsTimings,
    PyStatementRewriteSummary, normalize_policy_dimensions_py, normalize_policy_sources_py,
    parse_policy_to_json, parse_sql_to_ir, validate_constraint_expression_py,
};
use pyo3::prelude::*;

#[pymodule]
fn _passant(_py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyDfcPolicy>()?;
    module.add_class::<PyPlanner>()?;
    module.add_class::<PyRewriteStatsTimings>()?;
    module.add_class::<PyRewriteStats>()?;
    module.add_class::<PyStatementRewriteSummary>()?;
    module.add("PassantRewriteError", _py.get_type::<PassantRewriteError>())?;
    module.add_function(wrap_pyfunction!(parse_sql_to_ir, module)?)?;
    module.add_function(wrap_pyfunction!(parse_policy_to_json, module)?)?;
    module.add_function(wrap_pyfunction!(validate_constraint_expression_py, module)?)?;
    module.add_function(wrap_pyfunction!(normalize_policy_sources_py, module)?)?;
    module.add_function(wrap_pyfunction!(normalize_policy_dimensions_py, module)?)?;
    Ok(())
}
