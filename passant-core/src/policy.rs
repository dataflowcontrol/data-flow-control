use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

use crate::identifiers::{Alias, SourceName, TableName, normalize_key};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    Remove,
    Kill,
}

impl Resolution {
    pub fn parse(value: &str) -> Result<Self, PolicyParseError> {
        match value.trim().to_ascii_uppercase().as_str() {
            "REMOVE" => Ok(Self::Remove),
            "KILL" => Ok(Self::Kill),
            other => Err(PolicyParseError::InvalidResolution(other.to_string())),
        }
    }
}

#[derive(Debug, Error)]
pub enum PolicyParseError {
    #[error("policy text is empty")]
    Empty,
    #[error("missing required clause: {0}")]
    MissingClause(&'static str),
    #[error("invalid resolution: {0}")]
    InvalidResolution(String),
    #[error("invalid policy syntax: {0}")]
    InvalidSyntax(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyIr {
    Pgn {
        sources: Vec<String>,
        required_sources: Vec<String>,
        dimensions: Vec<String>,
        sink: Option<String>,
        sink_alias: Option<String>,
        #[serde(default)]
        source_aliases: HashMap<String, String>,
        constraint: String,
        on_fail: Resolution,
        description: Option<String>,
    },
}

impl PolicyIr {
    pub fn sources(&self) -> &[String] {
        match self {
            PolicyIr::Pgn { sources, .. } => sources,
        }
    }

    pub fn required_sources(&self) -> &[String] {
        match self {
            PolicyIr::Pgn {
                required_sources, ..
            } => required_sources,
        }
    }

    pub fn sink(&self) -> Option<&str> {
        match self {
            PolicyIr::Pgn { sink, .. } => sink.as_deref(),
        }
    }

    pub fn dimensions(&self) -> &[String] {
        match self {
            PolicyIr::Pgn { dimensions, .. } => dimensions,
        }
    }

    pub fn constraint(&self) -> &str {
        match self {
            PolicyIr::Pgn { constraint, .. } => constraint,
        }
    }

    pub fn resolution(&self) -> Resolution {
        match self {
            PolicyIr::Pgn { on_fail, .. } => *on_fail,
        }
    }

    pub fn name(&self) -> &'static str {
        "pgn"
    }

    pub fn source_aliases(&self) -> &HashMap<String, String> {
        match self {
            PolicyIr::Pgn { source_aliases, .. } => source_aliases,
        }
    }
}

#[derive(Debug)]
struct ParsedSources {
    sources: Vec<String>,
    required_sources: Vec<String>,
    aliases: HashMap<String, String>,
}

/// Normalize a policy source list (trim, reject empty/duplicate names).
pub fn normalize_policy_sources(sources: &[String]) -> Result<Vec<String>, PolicyParseError> {
    if sources.is_empty() {
        return Ok(Vec::new());
    }
    Ok(parse_sources(&sources.join(", "))?.sources)
}

/// Extract source alias → base table mappings from a policy source list.
pub fn normalize_policy_source_aliases(
    sources: &[String],
) -> Result<HashMap<String, String>, PolicyParseError> {
    if sources.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(parse_sources(&sources.join(", "))?.aliases)
}

/// Normalize a policy dimension list (trim, reject empty/duplicate names).
pub fn normalize_policy_dimensions(dimensions: &[String]) -> Result<Vec<String>, PolicyParseError> {
    if dimensions.is_empty() {
        return Ok(Vec::new());
    }
    let normalized = parse_dimensions(&dimensions.join(", "))?;
    let mut seen = HashSet::new();
    for dimension in &normalized {
        let key = normalize_key(dimension);
        if !seen.insert(key) {
            return Err(PolicyParseError::InvalidSyntax(format!(
                "Duplicate dimension '{dimension}' in dimensions list"
            )));
        }
    }
    Ok(normalized)
}

pub fn parse_policy_text(text: &str) -> Result<PolicyIr, PolicyParseError> {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return Err(PolicyParseError::Empty);
    }

    reject_legacy_policy_keywords(&normalized)?;

    let sources = clause_value(
        &normalized,
        &["SOURCE", "SOURCES"],
        &["SINK", "DIMENSION", "CONSTRAINT", "ON FAIL", "DESCRIPTION"],
    )
    .unwrap_or_default();
    let raw_sink = clause_value(
        &normalized,
        &["SINK"],
        &["DIMENSION", "CONSTRAINT", "ON FAIL", "DESCRIPTION"],
    )
    .and_then(blank_to_none);
    let (sink, sink_alias) = raw_sink
        .as_deref()
        .map(parse_name_alias)
        .transpose()?
        .unwrap_or((None, None));
    let dimensions = clause_value(
        &normalized,
        &["DIMENSION", "DIMENSIONS"],
        &["CONSTRAINT", "ON FAIL", "DESCRIPTION"],
    )
    .map(|value| parse_dimensions(&value))
    .transpose()?
    .unwrap_or_default();
    let constraint = clause_value(&normalized, &["CONSTRAINT"], &["ON FAIL", "DESCRIPTION"])
        .and_then(blank_to_none)
        .ok_or(PolicyParseError::MissingClause("CONSTRAINT"))?;
    let resolution = clause_value(&normalized, &["ON FAIL"], &["DESCRIPTION"])
        .and_then(blank_to_none)
        .ok_or(PolicyParseError::MissingClause("ON FAIL"))?;
    let description = clause_value(&normalized, &["DESCRIPTION"], &[]).and_then(blank_to_none);

    let parsed_sources = parse_sources(&sources)?;
    validate_sql_expression(&constraint, "constraint")?;
    for dimension in &dimensions {
        validate_sql_expression(dimension.as_str(), "dimension")?;
    }
    Ok(PolicyIr::Pgn {
        sources: parsed_sources.sources,
        required_sources: parsed_sources.required_sources,
        dimensions,
        sink,
        sink_alias,
        source_aliases: parsed_sources.aliases,
        constraint,
        on_fail: Resolution::parse(&resolution)?,
        description,
    })
}

fn reject_legacy_policy_keywords(normalized: &str) -> Result<(), PolicyParseError> {
    let upper = normalized.to_ascii_uppercase();
    if upper.starts_with("PGN ") || upper == "PGN" {
        return Err(PolicyParseError::InvalidSyntax(
            "the PGN keyword prefix was removed; policies begin with SOURCE, SINK, or CONSTRAINT clauses".into(),
        ));
    }
    if upper.starts_with("AGGREGATE ") || upper == "AGGREGATE" {
        return Err(PolicyParseError::InvalidSyntax(
            "the AGGREGATE clause was removed; put aggregate expressions in CONSTRAINT".into(),
        ));
    }
    if upper.starts_with("OVER ") || upper == "OVER" {
        return Err(PolicyParseError::InvalidSyntax(
            "the OVER keyword was removed; applicability is determined by SOURCE and SINK".into(),
        ));
    }
    if upper.starts_with("UPDATE ") || upper == "UPDATE" {
        return Err(PolicyParseError::InvalidSyntax(
            "the UPDATE keyword was removed; applicability is determined by SOURCE and SINK".into(),
        ));
    }
    Ok(())
}

fn clause_value(text: &str, starts: &[&str], ends: &[&str]) -> Option<String> {
    let upper = text.to_ascii_uppercase();
    let mut start_pos = None;
    let mut start_len = 0;
    for keyword in starts {
        let pattern = format!("{keyword} ");
        if let Some(pos) = upper.find(&pattern)
            && start_pos.is_none_or(|current| pos < current)
        {
            start_pos = Some(pos);
            start_len = pattern.len();
        }
    }
    let start = start_pos? + start_len;
    let mut end = text.len();
    for keyword in ends {
        let pattern = format!(" {keyword} ");
        if let Some(relative) = upper[start..].find(&pattern) {
            end = end.min(start + relative);
        }
    }
    Some(text[start..end].trim().to_string())
}

fn blank_to_none(value: String) -> Option<String> {
    if value.trim().is_empty() || value.eq_ignore_ascii_case("NONE") {
        None
    } else {
        Some(value)
    }
}

fn parse_sources(value: &str) -> Result<ParsedSources, PolicyParseError> {
    if value.trim().is_empty() || value.trim().eq_ignore_ascii_case("NONE") {
        return Ok(ParsedSources {
            sources: Vec::new(),
            required_sources: Vec::new(),
            aliases: HashMap::new(),
        });
    }
    let mut sources = Vec::new();
    let mut required_sources = Vec::new();
    let mut aliases = HashMap::new();
    let mut seen = HashSet::new();
    for raw in value.split(',') {
        let is_required = raw
            .split_whitespace()
            .any(|token| token.eq_ignore_ascii_case("REQUIRED"));
        let tokens = raw
            .split_whitespace()
            .filter(|token| !token.eq_ignore_ascii_case("REQUIRED"))
            .collect::<Vec<_>>();
        let (source, alias) = match tokens.as_slice() {
            [] => {
                return Err(PolicyParseError::InvalidSyntax(
                    "sources must be non-empty strings".into(),
                ));
            }
            [source] => (parse_table_reference(source)?, None),
            [source, as_keyword, alias] if as_keyword.eq_ignore_ascii_case("AS") => (
                parse_table_reference(source)?,
                Some(Alias::new(*alias).as_str().to_string()),
            ),
            _ => {
                return Err(PolicyParseError::InvalidSyntax(format!(
                    "invalid source list: {value}"
                )));
            }
        };
        if is_required {
            required_sources.push(source.clone());
        }
        if let Some(alias) = alias {
            aliases.insert(alias.to_ascii_lowercase(), source.clone());
        }
        let key = SourceName::parse(&source).key();
        if !seen.insert(key) {
            return Err(PolicyParseError::InvalidSyntax(format!(
                "duplicate source table '{source}' in sources list"
            )));
        }
        sources.push(source);
    }
    Ok(ParsedSources {
        sources,
        required_sources,
        aliases,
    })
}

fn parse_dimensions(value: &str) -> Result<Vec<String>, PolicyParseError> {
    if value.trim().is_empty() || value.trim().eq_ignore_ascii_case("NONE") {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|raw| {
            let dimension = raw.trim();
            if dimension.is_empty() {
                Err(PolicyParseError::InvalidSyntax(format!(
                    "invalid dimension list: {value}"
                )))
            } else {
                Ok(dimension.to_string())
            }
        })
        .collect()
}

fn validate_sql_expression(value: &str, label: &str) -> Result<(), PolicyParseError> {
    crate::sql::parse_policy_expr_duckdb(value).map_err(|err| {
        PolicyParseError::InvalidSyntax(format!("invalid {label} SQL expression '{value}': {err}"))
    })?;
    Ok(())
}

fn parse_table_reference(value: &str) -> Result<String, PolicyParseError> {
    let table = TableName::parse(value);
    if table.as_str().is_empty() {
        return Err(PolicyParseError::InvalidSyntax(
            "table name must be non-empty".into(),
        ));
    }
    Ok(table.as_str().to_string())
}

fn parse_name_alias(value: &str) -> Result<(Option<String>, Option<String>), PolicyParseError> {
    let tokens = value.split_whitespace().collect::<Vec<_>>();
    match tokens.as_slice() {
        [] => Ok((None, None)),
        [name] => Ok((Some(parse_table_reference(name)?), None)),
        [name, as_keyword, alias] if as_keyword.eq_ignore_ascii_case("AS") => Ok((
            Some(parse_table_reference(name)?),
            Some(Alias::new(*alias).as_str().to_string()),
        )),
        _ => Err(PolicyParseError::InvalidSyntax(format!(
            "invalid aliased name: {value}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_parsing_covers_all_supported_values() {
        assert_eq!(Resolution::parse("REMOVE").unwrap(), Resolution::Remove);
        assert_eq!(Resolution::parse("KILL").unwrap(), Resolution::Kill);
        assert!(Resolution::parse("LLM").is_err());
        assert!(Resolution::parse("UDF").is_err());
        assert!(Resolution::parse("INVALIDATE").is_err());
        assert!(Resolution::parse("INVALIDATE_MESSAGE").is_err());
        assert!(Resolution::parse("NOPE").is_err());
    }

    #[test]
    fn parse_sources_rejects_duplicate_tables() {
        let err = parse_sources("foo, foo").unwrap_err();
        assert!(err.to_string().contains("duplicate source"));
    }

    #[test]
    fn parse_table_reference_rejects_empty_name() {
        let err = parse_table_reference("  ").unwrap_err();
        assert!(err.to_string().contains("non-empty"));
    }

    #[test]
    fn normalize_policy_sources_rejects_duplicates() {
        let err = normalize_policy_sources(&["foo".to_string(), "Foo".to_string()]).unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn normalize_policy_dimensions_trims_entries() {
        let dims = normalize_policy_dimensions(&[" reports.region ".to_string()]).unwrap();
        assert_eq!(dims, ["reports.region"]);
    }

    #[test]
    fn normalize_policy_dimensions_rejects_duplicates() {
        let err = normalize_policy_dimensions(&[
            "reports.region".to_string(),
            "Reports.Region".to_string(),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("Duplicate dimension"));
    }

    #[test]
    fn parse_rejects_legacy_pgn_prefix() {
        let err = parse_policy_text(
            "PGN OVER SOURCE foo SINK reports AGGREGATE sum(foo.amount) CONSTRAINT sum(foo.amount) <= 1000 ON FAIL REMOVE",
        )
        .unwrap_err();
        assert!(err.to_string().contains("PGN keyword prefix was removed"));
    }

    #[test]
    fn parse_rejects_legacy_aggregate_prefix() {
        let err =
            parse_policy_text("AGGREGATE SOURCE foo CONSTRAINT max(foo.id) > 1 ON FAIL REMOVE")
                .unwrap_err();
        assert!(err.to_string().contains("AGGREGATE clause was removed"));
    }
}
