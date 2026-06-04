use pest::Parser;
use pest_derive::Parser;

use crate::policy::PolicyParseError;

#[derive(Parser)]
#[grammar = "policy_pgn.pest"]
struct PolicyPgnGrammar;

pub(crate) struct RawPgnPolicy {
    pub(crate) sources: Option<String>,
    pub(crate) sink: Option<String>,
    pub(crate) dimensions: Option<String>,
    pub(crate) constraint: String,
    pub(crate) on_fail: String,
    pub(crate) description: Option<String>,
}

pub(crate) fn parse_raw_pgn(normalized: &str) -> Result<RawPgnPolicy, PolicyParseError> {
    if normalized.is_empty() {
        return Err(PolicyParseError::Empty);
    }

    if !has_clause(normalized, &["CONSTRAINT"]) {
        return Err(PolicyParseError::MissingClause("CONSTRAINT"));
    }
    if !has_clause(normalized, &["ON FAIL"]) {
        return Err(PolicyParseError::MissingClause("ON FAIL"));
    }

    let mut pairs = PolicyPgnGrammar::parse(Rule::policy, normalized).map_err(map_pest_error)?;
    let policy = pairs
        .next()
        .ok_or_else(|| PolicyParseError::InvalidSyntax("expected policy".into()))?;

    let mut sources = None;
    let mut sink = None;
    let mut dimensions = None;
    let mut constraint = None;
    let mut on_fail = None;
    let mut description = None;

    for inner in policy.into_inner() {
        match inner.as_rule() {
            Rule::source_clause => sources = Some(extract_clause_body(inner, Rule::source_body)?),
            Rule::sink_clause => sink = Some(extract_clause_body(inner, Rule::sink_body)?),
            Rule::dimension_clause => {
                dimensions = Some(extract_clause_body(inner, Rule::dimension_body)?);
            }
            Rule::constraint_clause => {
                constraint = Some(extract_clause_body(inner, Rule::constraint_sql)?);
            }
            Rule::on_fail_clause => on_fail = Some(extract_clause_body(inner, Rule::on_fail_body)?),
            Rule::description_clause => {
                description = Some(extract_clause_body(inner, Rule::description_body)?);
            }
            Rule::EOI => {}
            _ => {}
        }
    }

    let constraint = constraint
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or(PolicyParseError::MissingClause("CONSTRAINT"))?;
    let on_fail = on_fail
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or(PolicyParseError::MissingClause("ON FAIL"))?;

    Ok(RawPgnPolicy {
        sources: sources
            .map(|value| value.trim().to_string())
            .filter(|v| !v.is_empty()),
        sink: sink
            .map(|value| value.trim().to_string())
            .filter(|v| !v.is_empty()),
        dimensions: dimensions
            .map(|value| value.trim().to_string())
            .filter(|v| !v.is_empty()),
        constraint,
        on_fail,
        description: description
            .map(|value| value.trim().to_string())
            .filter(|v| !v.is_empty()),
    })
}

fn extract_clause_body(
    clause: pest::iterators::Pair<'_, Rule>,
    body_rule: Rule,
) -> Result<String, PolicyParseError> {
    let body = clause
        .into_inner()
        .find(|pair| pair.as_rule() == body_rule)
        .ok_or_else(|| PolicyParseError::InvalidSyntax("missing clause body".into()))?;
    Ok(body.as_str().to_string())
}

fn map_pest_error(err: pest::error::Error<Rule>) -> PolicyParseError {
    let raw_message = err.variant.message();
    let message = raw_message.trim();
    if message.is_empty() || message == "expected policy" {
        PolicyParseError::InvalidSyntax(format!("invalid policy syntax: {err}"))
    } else {
        PolicyParseError::InvalidSyntax(format!("invalid policy syntax: {message}"))
    }
}

fn has_clause(text: &str, keywords: &[&str]) -> bool {
    let upper = text.to_ascii_uppercase();
    keywords.iter().any(|keyword| {
        if keyword.contains(' ') {
            upper.contains(keyword)
        } else {
            upper.split_whitespace().any(|token| token == *keyword)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> RawPgnPolicy {
        let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
        parse_raw_pgn(&normalized).expect("policy should parse")
    }

    #[test]
    fn constraint_span_ignores_on_fail_in_string_literal() {
        let raw = parse("SOURCE foo CONSTRAINT status = 'ON FAIL' ON FAIL REMOVE");
        assert_eq!(raw.constraint, "status = 'ON FAIL'");
        assert_eq!(raw.on_fail, "REMOVE");
    }

    #[test]
    fn constraint_span_ignores_description_in_string_literal() {
        let raw = parse("SOURCE foo CONSTRAINT col = 'DESCRIPTION foo' ON FAIL REMOVE");
        assert_eq!(raw.constraint, "col = 'DESCRIPTION foo'");
    }

    #[test]
    fn dimension_body_preserves_commas_inside_subquery() {
        let raw = parse(
            "SOURCE foo DIMENSION (SELECT id FROM t WHERE x IN (1, 2)) d CONSTRAINT max(foo.id) > 0 ON FAIL REMOVE",
        );
        assert_eq!(
            raw.dimensions.as_deref(),
            Some("(SELECT id FROM t WHERE x IN (1, 2)) d")
        );
    }

    #[test]
    fn constraint_span_ignores_on_fail_in_quoted_identifier() {
        let raw = parse(r#"SOURCE foo CONSTRAINT "ON FAIL" = 1 ON FAIL REMOVE"#);
        assert_eq!(raw.constraint, r#""ON FAIL" = 1"#);
    }
}
