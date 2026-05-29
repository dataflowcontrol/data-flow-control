use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

use crate::identifiers::{Alias, SourceName, TableName, normalize_key};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Resolution {
    Remove,
    Kill,
    Udf(String),
    RelationUdf(String),
}

impl Resolution {
    pub fn parse(value: &str) -> Result<Self, PolicyParseError> {
        let trimmed = value.trim();
        let upper = trimmed.to_ascii_uppercase();
        if upper == "REMOVE" {
            return Ok(Self::Remove);
        }
        if upper == "KILL" {
            return Ok(Self::Kill);
        }
        if upper.starts_with("LLM") {
            return Err(PolicyParseError::InvalidResolution(trimmed.to_string()));
        }
        if upper.starts_with("INVALIDATE") {
            return Err(PolicyParseError::InvalidResolution(trimmed.to_string()));
        }
        if let Some(name) = trimmed
            .strip_prefix("UDF ")
            .or_else(|| trimmed.strip_prefix("udf "))
        {
            let name = name.trim();
            if name.is_empty() {
                return Err(PolicyParseError::InvalidResolution(trimmed.to_string()));
            }
            return Ok(Self::Udf(name.to_string()));
        }
        if let Some(name) = trimmed
            .strip_prefix("RELATION UDF ")
            .or_else(|| trimmed.strip_prefix("relation udf "))
        {
            let name = name.trim();
            if name.is_empty() {
                return Err(PolicyParseError::InvalidResolution(trimmed.to_string()));
            }
            return Ok(Self::RelationUdf(name.to_string()));
        }
        Err(PolicyParseError::InvalidResolution(trimmed.to_string()))
    }

    pub fn as_label(&self) -> String {
        match self {
            Self::Remove => "REMOVE".to_string(),
            Self::Kill => "KILL".to_string(),
            Self::Udf(name) => format!("UDF {name}"),
            Self::RelationUdf(name) => format!("RELATION UDF {name}"),
        }
    }

    pub fn is_tuple_resolution(&self) -> bool {
        matches!(self, Self::Kill | Self::Udf(_))
    }

    pub fn is_relation_resolution(&self) -> bool {
        matches!(self, Self::RelationUdf(_))
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
        #[serde(alias = "dimensions")]
        dimension_tables: Vec<String>,
        #[serde(default)]
        dimension_aliases: HashMap<String, String>,
        #[serde(default)]
        dimension_queries: HashMap<String, String>,
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
        self.dimension_tables()
    }

    pub fn dimension_tables(&self) -> &[String] {
        match self {
            PolicyIr::Pgn {
                dimension_tables, ..
            } => dimension_tables,
        }
    }

    pub fn dimension_aliases(&self) -> &HashMap<String, String> {
        match self {
            PolicyIr::Pgn {
                dimension_aliases, ..
            } => dimension_aliases,
        }
    }

    pub fn dimension_queries(&self) -> &HashMap<String, String> {
        match self {
            PolicyIr::Pgn {
                dimension_queries, ..
            } => dimension_queries,
        }
    }

    pub fn constraint(&self) -> &str {
        match self {
            PolicyIr::Pgn { constraint, .. } => constraint,
        }
    }

    pub fn resolution(&self) -> Resolution {
        match self {
            PolicyIr::Pgn { on_fail, .. } => on_fail.clone(),
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

    pub fn sink_alias(&self) -> Option<&str> {
        match self {
            PolicyIr::Pgn { sink_alias, .. } => sink_alias.as_deref(),
        }
    }
}

#[derive(Debug)]
struct ParsedSources {
    sources: Vec<String>,
    required_sources: Vec<String>,
    aliases: HashMap<String, String>,
}

#[derive(Debug)]
struct ParsedDimensions {
    tables: Vec<String>,
    aliases: HashMap<String, String>,
    queries: HashMap<String, String>,
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

/// Normalize a policy dimension list (table+alias or subquery entries).
pub fn normalize_policy_dimensions(dimensions: &[String]) -> Result<Vec<String>, PolicyParseError> {
    if dimensions.is_empty() {
        return Ok(Vec::new());
    }
    let parsed = parse_dimensions(&dimensions.join(", "))?;
    let mut seen = HashSet::new();
    for table in &parsed.tables {
        let key = normalize_key(table);
        if !seen.insert(key) {
            return Err(PolicyParseError::InvalidSyntax(format!(
                "Duplicate dimension table '{table}' in dimensions list"
            )));
        }
    }
    Ok(parsed.tables)
}

pub fn normalize_policy_dimension_aliases(
    dimensions: &[String],
) -> Result<HashMap<String, String>, PolicyParseError> {
    if dimensions.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(parse_dimensions(&dimensions.join(", "))?.aliases)
}

pub fn normalize_policy_dimension_queries(
    dimensions: &[String],
) -> Result<HashMap<String, String>, PolicyParseError> {
    if dimensions.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(parse_dimensions(&dimensions.join(", "))?.queries)
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
    let dimension_clause = clause_value(
        &normalized,
        &["DIMENSION", "DIMENSIONS"],
        &["CONSTRAINT", "ON FAIL", "DESCRIPTION"],
    );
    let parsed_dimensions = dimension_clause
        .as_deref()
        .map(parse_dimensions)
        .transpose()?
        .unwrap_or(ParsedDimensions {
            tables: Vec::new(),
            aliases: HashMap::new(),
            queries: HashMap::new(),
        });
    let constraint = clause_value(&normalized, &["CONSTRAINT"], &["ON FAIL", "DESCRIPTION"])
        .and_then(blank_to_none)
        .ok_or(PolicyParseError::MissingClause("CONSTRAINT"))?;
    let resolution = clause_value(&normalized, &["ON FAIL"], &["DESCRIPTION"])
        .and_then(blank_to_none)
        .ok_or(PolicyParseError::MissingClause("ON FAIL"))?;
    let description = clause_value(&normalized, &["DESCRIPTION"], &[]).and_then(blank_to_none);

    let parsed_sources = parse_sources(&sources)?;
    validate_required_sources(&parsed_sources)?;
    let constraint = crate::rewriter::preprocess_policy_constraint(&constraint);
    validate_sql_expression(&constraint, "constraint")?;
    Ok(PolicyIr::Pgn {
        sources: parsed_sources.sources,
        required_sources: parsed_sources.required_sources,
        dimension_tables: parsed_dimensions.tables,
        dimension_aliases: parsed_dimensions.aliases,
        dimension_queries: parsed_dimensions.queries,
        sink,
        sink_alias,
        source_aliases: parsed_sources.aliases,
        constraint,
        on_fail: Resolution::parse(&resolution)?,
        description,
    })
}

fn validate_required_sources(parsed: &ParsedSources) -> Result<(), PolicyParseError> {
    let source_keys: HashSet<_> = parsed
        .sources
        .iter()
        .map(|source| normalize_key(source))
        .collect();
    for required in &parsed.required_sources {
        if !source_keys.contains(&normalize_key(required)) {
            return Err(PolicyParseError::InvalidSyntax(format!(
                "Required source '{required}' must also appear in sources list"
            )));
        }
    }
    Ok(())
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
    for raw in split_top_level_commas(value) {
        let is_required = raw
            .split_whitespace()
            .any(|token| token.eq_ignore_ascii_case("REQUIRED"));
        let tokens = raw
            .split_whitespace()
            .filter(|token| !token.eq_ignore_ascii_case("REQUIRED"))
            .collect::<Vec<_>>();
        let (source, alias) = parse_table_alias_tokens(&tokens, "source")?;
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

fn parse_dimensions(value: &str) -> Result<ParsedDimensions, PolicyParseError> {
    if value.trim().is_empty() || value.trim().eq_ignore_ascii_case("NONE") {
        return Ok(ParsedDimensions {
            tables: Vec::new(),
            aliases: HashMap::new(),
            queries: HashMap::new(),
        });
    }
    let mut tables = Vec::new();
    let mut aliases = HashMap::new();
    let mut queries = HashMap::new();
    for raw in split_top_level_commas(value) {
        let entry = raw.trim();
        if entry.is_empty() {
            return Err(PolicyParseError::InvalidSyntax(format!(
                "invalid dimension list: {value}"
            )));
        }
        if entry.starts_with('(') {
            let close = find_matching_paren(entry)?;
            let query = entry[..=close].to_string();
            validate_sql_expression(&query, "dimension query")?;
            let rest = entry[close + 1..].trim();
            let alias = parse_alias_only(rest)?;
            let key = alias.to_ascii_lowercase();
            aliases.insert(key.clone(), alias.clone());
            queries.insert(key, query);
            continue;
        }
        let tokens: Vec<&str> = entry.split_whitespace().collect();
        let (table, alias) = parse_table_alias_tokens(&tokens, "dimension")?;
        if let Some(alias) = alias {
            aliases.insert(alias.to_ascii_lowercase(), table.clone());
        }
        tables.push(table);
    }
    Ok(ParsedDimensions {
        tables,
        aliases,
        queries,
    })
}

fn parse_table_alias_tokens(
    tokens: &[&str],
    label: &str,
) -> Result<(String, Option<String>), PolicyParseError> {
    match tokens {
        [] => Err(PolicyParseError::InvalidSyntax(format!(
            "{label} entry must be non-empty"
        ))),
        [table] => Ok((parse_table_reference(table)?, None)),
        [table, as_keyword, alias] if as_keyword.eq_ignore_ascii_case("AS") => Ok((
            parse_table_reference(table)?,
            Some(Alias::new(*alias).as_str().to_string()),
        )),
        [table, alias] => Ok((
            parse_table_reference(table)?,
            Some(Alias::new(*alias).as_str().to_string()),
        )),
        _ => Err(PolicyParseError::InvalidSyntax(format!(
            "invalid {label} entry: {}",
            tokens.join(" ")
        ))),
    }
}

fn parse_alias_only(value: &str) -> Result<String, PolicyParseError> {
    let mut tokens = value.split_whitespace();
    let first = tokens.next().ok_or_else(|| {
        PolicyParseError::InvalidSyntax("dimension subquery requires an alias".into())
    })?;
    if first.eq_ignore_ascii_case("AS") {
        let alias = tokens.next().ok_or_else(|| {
            PolicyParseError::InvalidSyntax("dimension subquery requires an alias".into())
        })?;
        return Ok(Alias::new(alias).as_str().to_string());
    }
    if tokens.next().is_some() {
        return Err(PolicyParseError::InvalidSyntax(format!(
            "invalid dimension alias: {value}"
        )));
    }
    Ok(Alias::new(first).as_str().to_string())
}

fn split_top_level_commas(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for ch in value.chars() {
        match ch {
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => {
                parts.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }
    parts
}

fn find_matching_paren(value: &str) -> Result<usize, PolicyParseError> {
    let mut depth = 0usize;
    for (index, ch) in value.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Ok(index);
                }
            }
            _ => {}
        }
    }
    Err(PolicyParseError::InvalidSyntax(format!(
        "unbalanced parentheses in dimension entry: {value}"
    )))
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
    let tokens: Vec<&str> = value.split_whitespace().collect();
    match tokens.as_slice() {
        [] => Ok((None, None)),
        [name] => Ok((Some(parse_table_reference(name)?), None)),
        [name, as_keyword, alias] if as_keyword.eq_ignore_ascii_case("AS") => Ok((
            Some(parse_table_reference(name)?),
            Some(Alias::new(*alias).as_str().to_string()),
        )),
        [name, alias] => Ok((
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
        assert_eq!(
            Resolution::parse("UDF fix_row").unwrap(),
            Resolution::Udf("fix_row".into())
        );
        assert_eq!(
            Resolution::parse("RELATION UDF abort_batch").unwrap(),
            Resolution::RelationUdf("abort_batch".into())
        );
        assert!(Resolution::parse("LLM").is_err());
        assert!(Resolution::parse("INVALIDATE").is_err());
        assert!(Resolution::parse("UDF").is_err());
        assert!(Resolution::parse("NOPE").is_err());
    }

    #[test]
    fn parse_source_alias_without_as() {
        let parsed = parse_sources("Receipts R").unwrap();
        assert_eq!(parsed.sources, ["Receipts"]);
        assert_eq!(
            parsed.aliases.get("r").map(String::as_str),
            Some("Receipts")
        );
    }

    #[test]
    fn parse_dimension_table_alias() {
        let parsed = parse_dimensions("catalog_users U, catalog_roles R").unwrap();
        assert_eq!(parsed.tables, ["catalog_users", "catalog_roles"]);
        assert_eq!(
            parsed.aliases.get("u").map(String::as_str),
            Some("catalog_users")
        );
    }

    #[test]
    fn parse_policy_expands_unique_and_not_unique() {
        let unique = parse_policy_text("SOURCE users CONSTRAINT UNIQUE users.email ON FAIL REMOVE")
            .expect("unique policy");
        assert_eq!(unique.constraint(), "(COUNT(DISTINCT users.email) = 1)");

        let not_unique =
            parse_policy_text("SOURCE Receipts CONSTRAINT NOT UNIQUE Receipts.uid ON FAIL REMOVE")
                .expect("not unique policy");
        assert_eq!(
            not_unique.constraint(),
            "(COUNT(DISTINCT Receipts.uid) != 1)"
        );
    }

    #[test]
    fn parse_required_source_inline_adds_to_sources() {
        let policy = parse_policy_text(
            "SOURCE REQUIRED Receipts CONSTRAINT max(Receipts.id) > 0 ON FAIL REMOVE",
        )
        .expect("required inline source should parse");
        assert_eq!(policy.sources(), &["Receipts"]);
        assert_eq!(policy.required_sources(), &["Receipts"]);
    }
}
