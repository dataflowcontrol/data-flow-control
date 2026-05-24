use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::parser::ParseError;
use crate::policy::{PolicyParseError, Resolution};

/// Stable error classification for tests, explain output, and Python mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Parse,
    PolicyParse,
    UnknownTable,
    UnknownColumn,
    UnqualifiedColumn,
    UnaggregatedSourceColumn,
    InvalidSinkColumn,
    UnsupportedStatement,
    UnsupportedResolution,
    UnsupportedAggregate,
    UnsupportedPolicyCombination,
    RewriteStrategyUnavailable,
    CatalogValidation,
    InvariantViolation,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parse => "parse",
            Self::PolicyParse => "policy_parse",
            Self::UnknownTable => "unknown_table",
            Self::UnknownColumn => "unknown_column",
            Self::UnqualifiedColumn => "unqualified_column",
            Self::UnaggregatedSourceColumn => "unaggregated_source_column",
            Self::InvalidSinkColumn => "invalid_sink_column",
            Self::UnsupportedStatement => "unsupported_statement",
            Self::UnsupportedResolution => "unsupported_resolution",
            Self::UnsupportedAggregate => "unsupported_aggregate",
            Self::UnsupportedPolicyCombination => "unsupported_policy_combination",
            Self::RewriteStrategyUnavailable => "rewrite_strategy_unavailable",
            Self::CatalogValidation => "catalog_validation",
            Self::InvariantViolation => "invariant_violation",
        }
    }

    pub fn is_user_correctable(self) -> bool {
        matches!(
            self,
            Self::Parse
                | Self::PolicyParse
                | Self::UnknownTable
                | Self::UnknownColumn
                | Self::UnqualifiedColumn
                | Self::UnaggregatedSourceColumn
                | Self::InvalidSinkColumn
                | Self::CatalogValidation
                | Self::UnsupportedPolicyCombination
        )
    }
}

#[derive(Debug, Error)]
pub enum RewriteError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error(transparent)]
    PolicyParse(#[from] PolicyParseError),
    #[error("{message}")]
    Catalog {
        kind: ErrorKind,
        message: String,
        table: Option<String>,
        column: Option<String>,
    },
    #[error("unsupported query form: {message}")]
    Unsupported { kind: ErrorKind, message: String },
    #[error("unsupported resolution for SQL-only rewrite: {0:?}")]
    UnsupportedResolution(Resolution),
}

impl RewriteError {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Parse(_) => ErrorKind::Parse,
            Self::PolicyParse(_) => ErrorKind::PolicyParse,
            Self::Catalog { kind, .. } => *kind,
            Self::Unsupported { kind, .. } => *kind,
            Self::UnsupportedResolution(_) => ErrorKind::UnsupportedResolution,
        }
    }

    pub fn catalog(
        kind: ErrorKind,
        message: impl AsRef<str>,
        table: Option<String>,
        column: Option<String>,
    ) -> Self {
        Self::Catalog {
            kind,
            message: message.as_ref().to_string(),
            table,
            column,
        }
    }

    pub fn unsupported(kind: ErrorKind, message: String) -> Self {
        Self::Unsupported { kind, message }
    }

    pub fn unsupported_statement(message: impl Into<String>) -> Self {
        Self::unsupported(ErrorKind::UnsupportedStatement, message.into())
    }

    pub fn unsupported_aggregate(message: impl Into<String>) -> Self {
        Self::unsupported(ErrorKind::UnsupportedAggregate, message.into())
    }

    pub fn unsupported_policy_combination(message: impl Into<String>) -> Self {
        Self::unsupported(ErrorKind::UnsupportedPolicyCombination, message.into())
    }
}
