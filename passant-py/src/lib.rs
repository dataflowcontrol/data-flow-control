mod bindings;

use bindings::{
    PassantRewriteError, PyPlanner, PyRewriteStats, PyRewriteStatsTimings,
    PyStatementRewriteSummary, normalize_policy_dimension_aliases_py,
    normalize_policy_dimension_queries_py, normalize_policy_dimensions_py,
    normalize_policy_source_aliases_py, normalize_policy_sources_py, parse_policy_to_json,
    resolution_to_python_py, validate_constraint_expression_py,
};
use pyo3::prelude::*;

#[pymodule]
fn _passant(_py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyPlanner>()?;
    module.add_class::<PyRewriteStatsTimings>()?;
    module.add_class::<PyRewriteStats>()?;
    module.add_class::<PyStatementRewriteSummary>()?;
    module.add("PassantRewriteError", _py.get_type::<PassantRewriteError>())?;
    module.add_function(wrap_pyfunction!(parse_policy_to_json, module)?)?;
    module.add_function(wrap_pyfunction!(validate_constraint_expression_py, module)?)?;
    module.add_function(wrap_pyfunction!(normalize_policy_sources_py, module)?)?;
    module.add_function(wrap_pyfunction!(
        normalize_policy_source_aliases_py,
        module
    )?)?;
    module.add_function(wrap_pyfunction!(normalize_policy_dimensions_py, module)?)?;
    module.add_function(wrap_pyfunction!(
        normalize_policy_dimension_aliases_py,
        module
    )?)?;
    module.add_function(wrap_pyfunction!(
        normalize_policy_dimension_queries_py,
        module
    )?)?;
    module.add_function(wrap_pyfunction!(resolution_to_python_py, module)?)?;
    Ok(())
}
