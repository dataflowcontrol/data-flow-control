//! Lossless host scalar conversion only. Key comparison and correspondence live
//! in passant-core::tool_correspondence; no Python hashes or repr equality.
use passant_core::tool_correspondence::{ToolKey, ToolKeyScalar};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyFloat, PyInt, PyString};

fn exact_ratio(value: &Bound<'_, PyAny>, scalar_type: &Bound<'_, PyAny>) -> PyResult<ToolKeyScalar> {
    // Invoke the standard implementation rather than a subclass override.
    let ratio = scalar_type.getattr("as_integer_ratio")?.call1((value,))?;
    Ok(ToolKeyScalar::Number {
        numerator: ratio.get_item(0)?.str()?.to_str()?.to_owned(),
        denominator: ratio.get_item(1)?.str()?.to_str()?.to_owned(),
    })
}

fn encoded(scalar_type: &str, value: String) -> ToolKeyScalar {
    ToolKeyScalar::Encoded { scalar_type: scalar_type.into(), value }
}

pub(super) fn extract_key_scalar(value: &Bound<'_, PyAny>) -> PyResult<ToolKeyScalar> {
    let py = value.py();
    if value.is_none() { return Ok(ToolKeyScalar::Null); }
    if value.is_instance_of::<PyBool>() {
        return Ok(ToolKeyScalar::Number {
            numerator: if value.extract::<bool>()? { "1" } else { "0" }.into(),
            denominator: "1".into(),
        });
    }
    if value.is_instance_of::<PyInt>() {
        return exact_ratio(value, py.get_type::<PyInt>().as_any());
    }
    if value.is_instance_of::<PyFloat>() {
        let number = value.extract::<f64>()?;
        if number.is_nan() { return Ok(ToolKeyScalar::NaN); }
        if number.is_infinite() { return Ok(ToolKeyScalar::Infinity { negative: number.is_sign_negative() }); }
        return exact_ratio(value, py.get_type::<PyFloat>().as_any());
    }
    if value.is_instance_of::<PyString>() { return Ok(ToolKeyScalar::Text(value.extract()?)); }
    if let Ok(bytes) = value.downcast::<PyBytes>() { return Ok(ToolKeyScalar::Bytes(bytes.as_bytes().to_vec())); }

    let decimal_type = py.import("decimal")?.getattr("Decimal")?;
    if value.is_instance(&decimal_type)? {
        if decimal_type.getattr("is_nan")?.call1((value,))?.extract::<bool>()? { return Ok(ToolKeyScalar::NaN); }
        if decimal_type.getattr("is_infinite")?.call1((value,))?.extract::<bool>()? {
            return Ok(ToolKeyScalar::Infinity { negative: decimal_type.getattr("is_signed")?.call1((value,))?.extract()? });
        }
        return exact_ratio(value, &decimal_type);
    }

    let datetime = py.import("datetime")?;
    let datetime_type = datetime.getattr("datetime")?;
    if value.is_instance(&datetime_type)? {
        let offset = datetime_type.getattr("utcoffset")?.call1((value,))?;
        if offset.is_none() {
            return Ok(encoded("datetime_naive", datetime_type.getattr("isoformat")?.call1((value,))?.extract()?));
        }
        // Canonical instant representation, retaining all supported microseconds.
        let utc = datetime.getattr("timezone")?.getattr("utc")?;
        let canonical = datetime_type.getattr("astimezone")?.call1((value, utc))?;
        return Ok(encoded("datetime_utc", datetime_type.getattr("isoformat")?.call1((canonical,))?.extract()?));
    }
    let date_type = datetime.getattr("date")?;
    if value.is_instance(&date_type)? {
        return Ok(encoded("date", date_type.getattr("isoformat")?.call1((value,))?.extract()?));
    }
    let time_type = datetime.getattr("time")?;
    if value.is_instance(&time_type)? {
        let offset = time_type.getattr("utcoffset")?.call1((value,))?;
        let micros = ((value.getattr("hour")?.extract::<i64>()? * 60
            + value.getattr("minute")?.extract::<i64>()?) * 60
            + value.getattr("second")?.extract::<i64>()?) * 1_000_000
            + value.getattr("microsecond")?.extract::<i64>()?;
        if offset.is_none() { return Ok(encoded("time_naive", micros.to_string())); }
        let offset_micros = (offset.getattr("days")?.extract::<i64>()? * 86400
            + offset.getattr("seconds")?.extract::<i64>()?) * 1_000_000
            + offset.getattr("microseconds")?.extract::<i64>()?;
        // Do not wrap at midnight: datetime.time equality does not wrap a day.
        return Ok(encoded("time_utc", (micros - offset_micros).to_string()));
    }
    let uuid_type = py.import("uuid")?.getattr("UUID")?;
    if value.is_instance(&uuid_type)? { return Ok(encoded("uuid", value.getattr("hex")?.extract()?)); }
    Err(PyValueError::new_err("Output keys must contain hashable scalar values of supported SQL types"))
}

/// Convert occurrence keys, or select native-resolved key positions from rows.
/// Values outside the key are deliberately not inspected or converted.
pub(super) fn extract_key_rows(rows: &Bound<'_, PyAny>, positions: Option<&[usize]>) -> PyResult<Vec<ToolKey>> {
    rows.try_iter()?.map(|row| {
        let row = row?;
        match positions {
            Some(positions) => positions.iter().map(|index| extract_key_scalar(&row.get_item(*index)?)).collect(),
            None => row.try_iter()?.map(|value| extract_key_scalar(&value?)).collect(),
        }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use passant_core::tool_correspondence::{align_tool_keys, tool_key_positions, validate_tool_keys, ToolKeyRole};
    use std::ffi::CString;

    fn eval_keys(py: Python<'_>, code: &str) -> Vec<ToolKey> {
        let code = CString::new(code).unwrap();
        extract_key_rows(&py.eval(code.as_c_str(), None, None).unwrap(), None).unwrap()
    }

    #[test]
    fn host_numbers_preserve_exact_equality_without_repr() {
        Python::with_gil(|py| {
            let a = eval_keys(py, "[(True,), (0.5,), (10**40,)]");
            let b = eval_keys(py, "[(1,), (__import__('decimal').Decimal('0.5'),), (__import__('decimal').Decimal(10)**40,)]");
            assert_eq!(align_tool_keys(&a,&b,1).unwrap(),vec![0,1,2]);
            let a=eval_keys(py,"[(0.1,)]");
            let b=eval_keys(py,"[(__import__('decimal').Decimal('0.1'),)]");
            assert!(align_tool_keys(&a,&b,1).is_err());
            for code in ["[(float('nan'),)]", "[(__import__('decimal').Decimal('NaN'),)]", "[(None,)]"] {
                assert!(validate_tool_keys(&eval_keys(py,code),1,ToolKeyRole::Actual).is_err());
            }
        });
    }

    #[test]
    fn host_standard_temporal_uuid_and_bytes_keys_are_lossless() {
        Python::with_gil(|py| {
            let a=eval_keys(py,"[(__import__('datetime').datetime.fromisoformat('2026-01-01T10:00:00.123456+02:00'),),(__import__('datetime').date(2026,1,1),),(__import__('datetime').time.fromisoformat('10:00:00.123456+02:00'),),(__import__('uuid').UUID(int=42),),(b'\\x00\\xff',)]");
            let b=eval_keys(py,"[(__import__('datetime').datetime.fromisoformat('2026-01-01T08:00:00.123456+00:00'),),(__import__('datetime').date(2026,1,1),),(__import__('datetime').time.fromisoformat('08:00:00.123456+00:00'),),(__import__('uuid').UUID('00000000-0000-0000-0000-00000000002a'),),(b'\\x00\\xff',)]");
            assert_eq!(align_tool_keys(&a,&b,1).unwrap(),vec![0,1,2,3,4]);
        });
    }

    #[test]
    fn only_native_selected_key_columns_are_converted() {
        Python::with_gil(|py| {
            let positions=tool_key_positions(&["ID".into()], &["opaque".into(),"id".into()]).unwrap();
            let rows=py.eval(pyo3::ffi::c_str!("[(object(), 7)]"),None,None).unwrap();
            assert_eq!(extract_key_rows(&rows,Some(&positions)).unwrap(),eval_keys(py,"[(7,)]"));
            assert!(extract_key_rows(&rows,None).is_err());
        });
    }
}
