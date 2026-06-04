use passant_core::CatalogSnapshot;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

/// Deserialize a catalog snapshot from JSON produced by Python adapters.
pub fn deserialize_catalog_snapshot(catalog_json: &str) -> PyResult<CatalogSnapshot> {
    serde_json::from_str::<CatalogSnapshot>(catalog_json)
        .map_err(|err| PyValueError::new_err(err.to_string()))
}
