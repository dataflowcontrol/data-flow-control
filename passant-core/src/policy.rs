use serde::{Deserialize, Serialize};
use sqlparser::dialect::DuckDbDialect;
use sqlparser::parser::Parser;
use std::collections::HashMap;
use thiserror::Error;

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
pub enum FlowGuardPolicyKind {
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
pub struct FlowGuardPolicy {
    pub kind: FlowGuardPolicyKind,
    pub scope: PolicyScope,
    pub aggregations: Vec<String>,
    pub constraint: String,
    pub on_fail: Resolution,
    pub description: Option<String>,
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
    NativeFlowGuard(FlowGuardPolicy),
}

impl PolicyIr {
    pub fn sources(&self) -> &[String] {
        match self {
            PolicyIr::CompatDfc { sources, .. } => sources,
            PolicyIr::CompatAggregate(policy) => &policy.sources,
            PolicyIr::NativeFlowGuard(policy) => &policy.scope.sources,
        }
    }

    pub fn required_sources(&self) -> &[String] {
        match self {
            PolicyIr::CompatDfc {
                required_sources, ..
            } => required_sources,
            PolicyIr::CompatAggregate(_) | PolicyIr::NativeFlowGuard(_) => &[],
        }
    }

    pub fn sink(&self) -> Option<&str> {
        match self {
            PolicyIr::CompatDfc { sink, .. } => sink.as_deref(),
            PolicyIr::CompatAggregate(policy) => policy.sink.as_deref(),
            PolicyIr::NativeFlowGuard(policy) => policy.scope.sink.as_deref(),
        }
    }

    pub fn dimensions(&self) -> &[String] {
        match self {
            PolicyIr::CompatDfc { dimensions, .. } => dimensions,
            PolicyIr::CompatAggregate(policy) => &policy.dimensions,
            PolicyIr::NativeFlowGuard(policy) => &policy.scope.dimensions,
        }
    }

    pub fn constraint(&self) -> &str {
        match self {
            PolicyIr::CompatDfc { constraint, .. } => constraint,
            PolicyIr::CompatAggregate(policy) => &policy.constraint,
            PolicyIr::NativeFlowGuard(policy) => &policy.constraint,
        }
    }

    pub fn resolution(&self) -> Resolution {
        match self {
            PolicyIr::CompatDfc { on_fail, .. } => *on_fail,
            PolicyIr::CompatAggregate(_) => Resolution::Invalidate,
            PolicyIr::NativeFlowGuard(policy) => policy.on_fail,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            PolicyIr::CompatDfc { .. } => "compat_dfc",
            PolicyIr::CompatAggregate(_) => "compat_aggregate",
            PolicyIr::NativeFlowGuard(_) => "flowguard",
        }
    }
}

struct ParsedSources {
    sources: Vec<String>,
    required_sources: Vec<String>,
    aliases: HashMap<String, String>,
}

pub fn parse_policy_text(text: &str) -> Result<PolicyIr, PolicyParseError> {
    let mut normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return Err(PolicyParseError::Empty);
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
    for raw in value.split(',') {
        let is_required = raw
            .split_whitespace()
            .any(|token| token.eq_ignore_ascii_case("REQUIRED"));
        let tokens = raw
            .split_whitespace()
            .filter(|token| !token.eq_ignore_ascii_case("REQUIRED"))
            .collect::<Vec<_>>();
        let (source, alias) = match tokens.as_slice() {
            [source] => ((*source).to_string(), None),
            [source, as_keyword, alias] if as_keyword.eq_ignore_ascii_case("AS") => {
                ((*source).to_string(), Some((*alias).to_string()))
            }
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

fn parse_name_alias(value: &str) -> Result<(Option<String>, Option<String>), PolicyParseError> {
    let tokens = value.split_whitespace().collect::<Vec<_>>();
    match tokens.as_slice() {
        [] => Ok((None, None)),
        [name] => Ok((Some((*name).to_string()), None)),
        [name, as_keyword, alias] if as_keyword.eq_ignore_ascii_case("AS") => {
            Ok((Some((*name).to_string()), Some((*alias).to_string())))
        }
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
}
