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
    UnsupportedDialect,
    UnsupportedBackendCapability,
    DialectParseFailure,
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
            Self::UnsupportedDialect => "unsupported_dialect",
            Self::UnsupportedBackendCapability => "unsupported_backend_capability",
            Self::DialectParseFailure => "dialect_parse_failure",
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

/// Catalog or constraint-syntax validation failure with structured context.
#[derive(Debug)]
pub struct CatalogErrorDetails {
    pub kind: ErrorKind,
    pub message: String,
    pub table: Option<String>,
    pub column: Option<String>,
    pub constraint: Option<String>,
    pub validation_phase: Option<String>,
}

#[derive(Debug, Error)]
pub enum RewriteError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error(transparent)]
    PolicyParse(#[from] PolicyParseError),
    #[error("{}", .0.message)]
    Catalog(Box<CatalogErrorDetails>),
    #[error("unsupported query form: {message}")]
    Unsupported { kind: ErrorKind, message: String },
    #[error("unsupported resolution for SQL-only rewrite: {0:?}")]
    UnsupportedResolution(Resolution),
}

impl RewriteError {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Parse(err) => match err {
                ParseError::Sql(_) => ErrorKind::DialectParseFailure,
                ParseError::ExpectedSingleStatement | ParseError::Unsupported(_) => {
                    ErrorKind::Parse
                }
            },
            Self::PolicyParse(_) => ErrorKind::PolicyParse,
            Self::Catalog(details) => details.kind,
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
        Self::catalog_with_context(kind, message, table, column, None, None)
    }

    pub fn catalog_with_context(
        kind: ErrorKind,
        message: impl AsRef<str>,
        table: Option<String>,
        column: Option<String>,
        constraint: Option<String>,
        validation_phase: Option<&'static str>,
    ) -> Self {
        Self::Catalog(Box::new(CatalogErrorDetails {
            kind,
            message: message.as_ref().to_string(),
            table,
            column,
            constraint,
            validation_phase: validation_phase.map(str::to_string),
        }))
    }

    pub fn unsupported_dialect(message: impl Into<String>) -> Self {
        Self::unsupported(ErrorKind::UnsupportedDialect, message.into())
    }

    pub fn unsupported_backend_capability(message: impl Into<String>) -> Self {
        Self::unsupported(ErrorKind::UnsupportedBackendCapability, message.into())
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
