use serde::{Deserialize, Serialize};
use sqlparser::dialect::DuckDbDialect;
use sqlparser::parser::Parser;
use std::collections::{HashMap, HashSet};
use thiserror::Error;

use crate::identifiers::{Alias, SourceName, TableName, normalize_key};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    Remove,
    Kill,
    Invalidate,
    InvalidateMessage,
    Llm,
}

impl Resolution {
    pub fn parse(value: &str) -> Result<Self, PolicyParseError> {
        match value.trim().to_ascii_uppercase().as_str() {
            "REMOVE" => Ok(Self::Remove),
            "KILL" => Ok(Self::Kill),
            "INVALIDATE" => Ok(Self::Invalidate),
            "INVALIDATE_MESSAGE" => Ok(Self::InvalidateMessage),
            "LLM" | "UDF" => Ok(Self::Llm),
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
pub enum PgnPolicyKind {
    Over,
    Update,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyScope {
    pub sources: Vec<String>,
    pub sink: Option<String>,
    pub sink_alias: Option<String>,
    pub dimensions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PgnPolicy {
    pub kind: PgnPolicyKind,
    pub scope: PolicyScope,
    pub aggregations: Vec<String>,
    pub constraint: String,
    pub on_fail: Resolution,
    pub description: Option<String>,
    /// Original policy text when registered via `register_policy_text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregateDfcPolicy {
    pub sources: Vec<String>,
    #[serde(default)]
    pub dimensions: Vec<String>,
    pub sink: Option<String>,
    pub constraint: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyIr {
    CompatDfc {
        sources: Vec<String>,
        required_sources: Vec<String>,
        dimensions: Vec<String>,
        sink: Option<String>,
        sink_alias: Option<String>,
        constraint: String,
        on_fail: Resolution,
        description: Option<String>,
    },
    CompatAggregate(AggregateDfcPolicy),
    NativePgn(PgnPolicy),
}

impl PolicyIr {
    pub fn sources(&self) -> &[String] {
        match self {
            PolicyIr::CompatDfc { sources, .. } => sources,
            PolicyIr::CompatAggregate(policy) => &policy.sources,
            PolicyIr::NativePgn(policy) => &policy.scope.sources,
        }
    }

    pub fn required_sources(&self) -> &[String] {
        match self {
            PolicyIr::CompatDfc {
                required_sources, ..
            } => required_sources,
            PolicyIr::CompatAggregate(_) | PolicyIr::NativePgn(_) => &[],
        }
    }

    pub fn sink(&self) -> Option<&str> {
        match self {
            PolicyIr::CompatDfc { sink, .. } => sink.as_deref(),
            PolicyIr::CompatAggregate(policy) => policy.sink.as_deref(),
            PolicyIr::NativePgn(policy) => policy.scope.sink.as_deref(),
        }
    }

    pub fn dimensions(&self) -> &[String] {
        match self {
            PolicyIr::CompatDfc { dimensions, .. } => dimensions,
            PolicyIr::CompatAggregate(policy) => &policy.dimensions,
            PolicyIr::NativePgn(policy) => &policy.scope.dimensions,
        }
    }

    pub fn constraint(&self) -> &str {
        match self {
            PolicyIr::CompatDfc { constraint, .. } => constraint,
            PolicyIr::CompatAggregate(policy) => &policy.constraint,
            PolicyIr::NativePgn(policy) => &policy.constraint,
        }
    }

    pub fn resolution(&self) -> Resolution {
        match self {
            PolicyIr::CompatDfc { on_fail, .. } => *on_fail,
            PolicyIr::CompatAggregate(_) => Resolution::Invalidate,
            PolicyIr::NativePgn(policy) => policy.on_fail,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            PolicyIr::CompatDfc { .. } => "compat_dfc",
            PolicyIr::CompatAggregate(_) => "compat_aggregate",
            PolicyIr::NativePgn(_) => "pgn",
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
    let mut normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return Err(PolicyParseError::Empty);
    }

    if strip_keyword_prefix(&mut normalized, "PGN") {
        return parse_pgn_policy(&normalized);
    }

    let is_aggregate = strip_keyword_prefix(&mut normalized, "AGGREGATE");
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
    let mut constraint = clause_value(&normalized, &["CONSTRAINT"], &["ON FAIL", "DESCRIPTION"])
        .and_then(blank_to_none)
        .ok_or(PolicyParseError::MissingClause("CONSTRAINT"))?;
    let resolution = clause_value(&normalized, &["ON FAIL"], &["DESCRIPTION"])
        .and_then(blank_to_none)
        .ok_or(PolicyParseError::MissingClause("ON FAIL"))?;
    let description = clause_value(&normalized, &["DESCRIPTION"], &[]).and_then(blank_to_none);

    let parsed_sources = parse_sources(&sources)?;
    constraint = rewrite_constraint_source_aliases(&constraint, &parsed_sources.aliases);
    let dimensions: Vec<String> = dimensions
        .into_iter()
        .map(|dimension| rewrite_constraint_source_aliases(&dimension, &parsed_sources.aliases))
        .collect();
    validate_sql_expression(&constraint, "constraint")?;
    for dimension in &dimensions {
        validate_sql_expression(dimension.as_str(), "dimension")?;
    }
    if is_aggregate {
        let on_fail = Resolution::parse(&resolution)?;
        if on_fail != Resolution::Invalidate {
            return Err(PolicyParseError::InvalidSyntax(
                "aggregate policies currently only support INVALIDATE resolution".into(),
            ));
        }
        return Ok(PolicyIr::CompatAggregate(AggregateDfcPolicy {
            sources: parsed_sources.sources,
            dimensions,
            sink,
            constraint,
            description,
        }));
    }

    Ok(PolicyIr::CompatDfc {
        sources: parsed_sources.sources,
        required_sources: parsed_sources.required_sources,
        dimensions,
        sink,
        sink_alias,
        constraint,
        on_fail: Resolution::parse(&resolution)?,
        description,
    })
}

fn parse_pgn_policy(normalized: &str) -> Result<PolicyIr, PolicyParseError> {
    let mut remainder = normalized.to_string();
    let kind = if strip_keyword_prefix(&mut remainder, "OVER") {
        PgnPolicyKind::Over
    } else if strip_keyword_prefix(&mut remainder, "UPDATE") {
        PgnPolicyKind::Update
    } else {
        return Err(PolicyParseError::MissingClause("OVER or UPDATE"));
    };
    let sources = clause_value(
        &remainder,
        &["SOURCE", "SOURCES"],
        &[
            "SINK",
            "DIMENSION",
            "AGGREGATE",
            "CONSTRAINT",
            "ON FAIL",
            "DESCRIPTION",
        ],
    )
    .unwrap_or_default();
    let raw_sink = clause_value(
        &remainder,
        &["SINK"],
        &[
            "DIMENSION",
            "AGGREGATE",
            "CONSTRAINT",
            "ON FAIL",
            "DESCRIPTION",
        ],
    )
    .and_then(blank_to_none);
    let (sink, sink_alias) = raw_sink
        .as_deref()
        .map(parse_name_alias)
        .transpose()?
        .unwrap_or((None, None));
    let dimensions = clause_value(
        &remainder,
        &["DIMENSION", "DIMENSIONS"],
        &["AGGREGATE", "CONSTRAINT", "ON FAIL", "DESCRIPTION"],
    )
    .map(|value| parse_dimensions(&value))
    .transpose()?
    .unwrap_or_default();
    let aggregations = clause_value(
        &remainder,
        &["AGGREGATE"],
        &["CONSTRAINT", "ON FAIL", "DESCRIPTION"],
    )
    .and_then(blank_to_none)
    .ok_or(PolicyParseError::MissingClause("AGGREGATE"))?;
    let mut constraint = clause_value(&remainder, &["CONSTRAINT"], &["ON FAIL", "DESCRIPTION"])
        .and_then(blank_to_none)
        .ok_or(PolicyParseError::MissingClause("CONSTRAINT"))?;
    let resolution = clause_value(&remainder, &["ON FAIL"], &["DESCRIPTION"])
        .and_then(blank_to_none)
        .ok_or(PolicyParseError::MissingClause("ON FAIL"))?;
    let description = clause_value(&remainder, &["DESCRIPTION"], &[]).and_then(blank_to_none);

    let parsed_sources = parse_sources(&sources)?;
    constraint = rewrite_constraint_source_aliases(&constraint, &parsed_sources.aliases);
    let dimensions: Vec<String> = dimensions
        .into_iter()
        .map(|dimension| rewrite_constraint_source_aliases(&dimension, &parsed_sources.aliases))
        .collect();
    validate_sql_expression(&constraint, "constraint")?;
    for dimension in &dimensions {
        validate_sql_expression(dimension.as_str(), "dimension")?;
    }
    for aggregation in aggregations.split(',') {
        validate_sql_expression(aggregation.trim(), "aggregate")?;
    }

    Ok(PolicyIr::NativePgn(PgnPolicy {
        kind,
        scope: PolicyScope {
            sources: parsed_sources.sources,
            sink,
            sink_alias,
            dimensions,
        },
        aggregations: aggregations
            .split(',')
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect(),
        constraint,
        on_fail: Resolution::parse(&resolution)?,
        description,
        source_text: None,
    }))
}

fn strip_keyword_prefix(text: &mut String, keyword: &str) -> bool {
    let prefix = format!("{keyword} ");
    if text.to_ascii_uppercase().starts_with(&prefix) {
        *text = text[prefix.len()..].trim().to_string();
        true
    } else {
        false
    }
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
    Parser::parse_sql(&DuckDbDialect {}, &format!("SELECT {value}"))
        .map(|_| ())
        .map_err(|err| {
            PolicyParseError::InvalidSyntax(format!(
                "invalid {label} SQL expression '{value}': {err}"
            ))
        })
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

fn rewrite_constraint_source_aliases(
    constraint: &str,
    aliases: &HashMap<String, String>,
) -> String {
    if aliases.is_empty() {
        return constraint.to_string();
    }
    let mut rewritten = String::with_capacity(constraint.len());
    let chars = constraint.chars().collect::<Vec<_>>();
    let mut index = 0;
    let mut in_string = false;
    while index < chars.len() {
        let current = chars[index];
        if current == '\'' {
            in_string = !in_string;
            rewritten.push(current);
            index += 1;
            continue;
        }
        if !in_string && is_identifier_start(current) {
            let start = index;
            index += 1;
            while index < chars.len() && is_identifier_continue(chars[index]) {
                index += 1;
            }
            let ident = chars[start..index].iter().collect::<String>();
            if index < chars.len()
                && chars[index] == '.'
                && let Some(source) = aliases.get(&ident.to_ascii_lowercase())
            {
                rewritten.push_str(source);
            } else {
                rewritten.push_str(&ident);
            }
            continue;
        }
        rewritten.push(current);
        index += 1;
    }
    rewritten
}

fn is_identifier_start(value: char) -> bool {
    value == '_' || value.is_ascii_alphabetic()
}

fn is_identifier_continue(value: char) -> bool {
    value == '_' || value.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_parsing_covers_all_supported_values() {
        assert_eq!(Resolution::parse("REMOVE").unwrap(), Resolution::Remove);
        assert_eq!(Resolution::parse("KILL").unwrap(), Resolution::Kill);
        assert_eq!(Resolution::parse("UDF").unwrap(), Resolution::Llm);
        assert!(Resolution::parse("NOPE").is_err());
    }

    #[test]
    fn aggregate_keyword_parses_into_compat_aggregate_policy() {
        let policy = parse_policy_text(
            "AGGREGATE SOURCE foo SINK reports CONSTRAINT sum(reports.total) > 100 ON FAIL INVALIDATE",
        )
        .expect("aggregate policy should parse");
        assert!(matches!(policy, PolicyIr::CompatAggregate(_)));
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
}
