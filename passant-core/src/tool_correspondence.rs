//! Backend-independent occurrence correspondence for trusted tool outputs.
//!
//! Host bindings encode scalar values losslessly. Equality, uniqueness, result
//! schema lookup and whole-state row alignment belong here, not in a host policy
//! engine. SQL plans additionally validate backend collation/coercion semantics.
use std::collections::HashMap;

use num_bigint::BigInt;
use num_rational::BigRational;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_casefold::UnicodeCaseFold;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ToolKeyScalar {
    Null,
    NaN,
    Infinity {
        negative: bool,
    },
    /// Exact finite numeric representation, including integers, booleans,
    /// IEEE floating-point values and decimals. The host must not round it.
    Number {
        numerator: String,
        denominator: String,
    },
    Text(String),
    Bytes(Vec<u8>),
    /// Lossless standard-type encoding (date/time/datetime/UUID, for example).
    /// Type identity is retained; arbitrary object repr strings are not keys.
    Encoded {
        scalar_type: String,
        value: String,
    },
}

pub type ToolKey = Vec<ToolKeyScalar>;

#[derive(Debug, Clone, Copy)]
pub enum ToolKeyRole {
    Modeled,
    Actual,
}

impl ToolKeyRole {
    fn label(self) -> &'static str {
        match self {
            Self::Modeled => "Modeled",
            Self::Actual => "Actual",
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{0}")]
pub struct ToolCorrespondenceError(pub String);

type Result<T> = std::result::Result<T, ToolCorrespondenceError>;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum NormalizedScalar {
    Number(BigRational),
    Infinity(bool),
    Text(String),
    Bytes(Vec<u8>),
    Encoded(String, String),
}

fn normalize(value: &ToolKeyScalar, role: ToolKeyRole) -> Result<NormalizedScalar> {
    let invalid_number = || {
        ToolCorrespondenceError(format!(
            "{} output keys must contain valid exact numeric scalar values",
            role.label()
        ))
    };
    Ok(match value {
        ToolKeyScalar::Null | ToolKeyScalar::NaN => {
            return Err(ToolCorrespondenceError(format!(
                "{} output keys must not contain NULL or NaN",
                role.label()
            )));
        }
        ToolKeyScalar::Number {
            numerator,
            denominator,
        } => {
            let numerator: BigInt = numerator.parse().map_err(|_| invalid_number())?;
            let denominator: BigInt = denominator.parse().map_err(|_| invalid_number())?;
            if denominator == BigInt::from(0) {
                return Err(invalid_number());
            }
            NormalizedScalar::Number(BigRational::new(numerator, denominator))
        }
        ToolKeyScalar::Infinity { negative } => NormalizedScalar::Infinity(*negative),
        ToolKeyScalar::Text(value) => NormalizedScalar::Text(value.clone()),
        ToolKeyScalar::Bytes(value) => NormalizedScalar::Bytes(value.clone()),
        ToolKeyScalar::Encoded { scalar_type, value } => {
            if scalar_type.is_empty() {
                return Err(ToolCorrespondenceError(
                    "Encoded scalar key type must be nonempty".into(),
                ));
            }
            NormalizedScalar::Encoded(scalar_type.clone(), value.clone())
        }
    })
}

fn key_index(
    keys: &[ToolKey],
    arity: usize,
    role: ToolKeyRole,
) -> Result<HashMap<Vec<NormalizedScalar>, usize>> {
    let mut index = HashMap::with_capacity(keys.len());
    for (position, key) in keys.iter().enumerate() {
        if key.len() != arity {
            return Err(ToolCorrespondenceError(match role {
                ToolKeyRole::Actual => "Output key arity does not match key_columns".into(),
                ToolKeyRole::Modeled => "Modeled key arity does not match key_columns".into(),
            }));
        }
        let normalized = key
            .iter()
            .map(|value| normalize(value, role))
            .collect::<Result<Vec<_>>>()?;
        if index.insert(normalized, position).is_some() {
            return Err(ToolCorrespondenceError(format!(
                "{} output keys must be unique; distinguish duplicate occurrences",
                role.label()
            )));
        }
    }
    Ok(index)
}

/// Validate one side before contacting a backend or publishing an observation.
pub fn validate_tool_keys(keys: &[ToolKey], arity: usize, role: ToolKeyRole) -> Result<()> {
    key_index(keys, arity, role).map(|_| ())
}

/// Return the modeled row index for each actual output, after exact bijection.
/// An empty key is a singleton occurrence, not a wildcard or an unordered zip.
pub fn align_tool_keys(
    modeled: &[ToolKey],
    actual: &[ToolKey],
    arity: usize,
) -> Result<Vec<usize>> {
    let modeled_index = key_index(modeled, arity, ToolKeyRole::Modeled)?;
    let actual_index = key_index(actual, arity, ToolKeyRole::Actual)?;
    if modeled_index.len() != actual_index.len()
        || actual_index
            .keys()
            .any(|key| !modeled_index.contains_key(key))
    {
        return Err(ToolCorrespondenceError(
            "Actual outputs do not match all modeled output keys".into(),
        ));
    }
    let mut alignment = vec![0; actual.len()];
    for (key, actual_position) in actual_index {
        alignment[actual_position] = modeled_index[&key];
    }
    Ok(alignment)
}

/// Resolve key columns once. Bindings may then copy only these typed values;
/// irrelevant payload columns need not be converted or inspected.
pub fn tool_key_positions(key_columns: &[String], result_columns: &[String]) -> Result<Vec<usize>> {
    let columns: Vec<String> = result_columns
        .iter()
        .map(|name| name.case_fold().collect())
        .collect();
    key_columns
        .iter()
        .map(|key| {
            let key: String = key.case_fold().collect();
            let mut found = columns.iter().enumerate().filter(|(_, name)| **name == key);
            match (found.next(), found.next()) {
                (Some((index, _)), None) => Ok(index),
                _ => Err(ToolCorrespondenceError(
                    "Modeled key columns must identify unique result columns".into(),
                )),
            }
        })
        .collect()
}

/// A partially consumed result cannot be aligned to its original state vector.
pub fn validate_tool_result_alignment(
    row_count: usize,
    state_count: Option<usize>,
    has_description: bool,
) -> Result<()> {
    let Some(state_count) = state_count.filter(|_| has_description) else {
        return Err(ToolCorrespondenceError(
            "Model result must carry automatic intermediate states".into(),
        ));
    };
    if row_count != state_count {
        return Err(ToolCorrespondenceError(
            "Model result must be fresh and have one intermediate tuple per row".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn number(n: &str, d: &str) -> ToolKeyScalar {
        ToolKeyScalar::Number {
            numerator: n.into(),
            denominator: d.into(),
        }
    }
    fn text(s: &str) -> ToolKeyScalar {
        ToolKeyScalar::Text(s.into())
    }

    #[test]
    fn occurrence_bijection_reorders_and_preserves_duplicate_payload_independence() {
        assert_eq!(
            align_tool_keys(
                &[vec![text("a")], vec![text("b")]],
                &[vec![text("b")], vec![text("a")]],
                1
            )
            .unwrap(),
            vec![1, 0]
        );
        assert!(
            align_tool_keys(&[vec![text("a")]], &[vec![text("b")]], 1)
                .unwrap_err()
                .0
                .contains("do not match")
        );
        assert!(align_tool_keys(&[vec![text("a")]], &[], 1).is_err());
        assert_eq!(align_tool_keys(&[vec![]], &[vec![]], 0).unwrap(), vec![0]);
        assert!(align_tool_keys(&[vec![], vec![]], &[vec![]], 0).is_err());
        assert_eq!(align_tool_keys(&[], &[], 0).unwrap(), Vec::<usize>::new());
    }

    #[test]
    fn numeric_equality_is_exact_across_host_scalar_types() {
        assert_eq!(
            align_tool_keys(&[vec![number("1", "1")]], &[vec![number("10", "10")]], 1).unwrap(),
            vec![0]
        );
        assert!(
            validate_tool_keys(
                &[vec![number("1", "1")], vec![number("2", "2")]],
                1,
                ToolKeyRole::Actual
            )
            .is_err()
        );
        // Decimal('0.1') differs from the exact IEEE binary value of float(0.1).
        assert!(
            align_tool_keys(
                &[vec![number("1", "10")]],
                &[vec![number("3602879701896397", "36028797018963968")]],
                1
            )
            .is_err()
        );
        assert_eq!(
            align_tool_keys(&[vec![number("0", "1")]], &[vec![number("0", "-2")]], 1).unwrap(),
            vec![0]
        );
        assert_eq!(
            align_tool_keys(
                &[vec![number("184467440737095516160000", "1")]],
                &[vec![number("1844674407370955161600000", "10")]],
                1
            )
            .unwrap(),
            vec![0]
        );
    }

    #[test]
    fn null_nan_and_invalid_numbers_fail_closed() {
        for value in [ToolKeyScalar::Null, ToolKeyScalar::NaN] {
            assert!(
                validate_tool_keys(&[vec![value]], 1, ToolKeyRole::Actual)
                    .unwrap_err()
                    .0
                    .contains("NULL or NaN")
            );
        }
        for value in [number("1", "0"), number("invalid", "1")] {
            assert!(validate_tool_keys(&[vec![value]], 1, ToolKeyRole::Actual).is_err());
        }
        assert!(
            validate_tool_keys(&[vec![]], 1, ToolKeyRole::Actual)
                .unwrap_err()
                .0
                .contains("arity")
        );
    }

    #[test]
    fn typed_scalar_values_are_not_repr_or_string_coercions() {
        assert!(align_tool_keys(&[vec![text("1")]], &[vec![number("1", "1")]], 1).is_err());
        assert!(
            align_tool_keys(
                &[vec![text("a")]],
                &[vec![ToolKeyScalar::Bytes(vec![b'a'])]],
                1
            )
            .is_err()
        );
        assert!(
            align_tool_keys(
                &[vec![ToolKeyScalar::Infinity { negative: false }]],
                &[vec![ToolKeyScalar::Infinity { negative: true }]],
                1
            )
            .is_err()
        );
        assert!(
            align_tool_keys(
                &[vec![ToolKeyScalar::Encoded {
                    scalar_type: "date".into(),
                    value: "2026-01-01".into()
                }]],
                &[vec![text("2026-01-01")]],
                1
            )
            .is_err()
        );
    }

    #[test]
    fn schema_lookup_uses_native_unicode_casefold_and_rejects_ambiguity() {
        assert_eq!(
            tool_key_positions(
                &["STRASSE".into(), "id".into()],
                &["other".into(), "Straße".into(), "ID".into()]
            )
            .unwrap(),
            vec![1, 2]
        );
        assert!(tool_key_positions(&["id".into()], &["ID".into(), "id".into()]).is_err());
        assert!(tool_key_positions(&["missing".into()], &["id".into()]).is_err());
    }

    #[test]
    fn whole_state_alignment_requires_complete_materialized_result() {
        assert!(
            validate_tool_result_alignment(1, Some(2), true)
                .unwrap_err()
                .0
                .contains("fresh")
        );
        assert!(
            validate_tool_result_alignment(0, None, true)
                .unwrap_err()
                .0
                .contains("automatic")
        );
        assert!(validate_tool_result_alignment(0, Some(0), false).is_err());
        assert!(validate_tool_result_alignment(0, Some(0), true).is_ok());
    }
}
