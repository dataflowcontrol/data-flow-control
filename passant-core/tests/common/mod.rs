#![allow(dead_code)]

use passant_core::{
    PassantPlanner, PassantRewriter, PlanQueryResult, PolicyIr, Resolution, RewriteError,
    RewriteStrategy, TableCatalog, parse_query_to_ir,
};

pub fn pgn_policy(sources: &[&str], constraint: &str) -> PolicyIr {
    pgn_policy_with(sources, constraint, Resolution::Remove)
}

pub fn pgn_policy_with(sources: &[&str], constraint: &str, on_fail: Resolution) -> PolicyIr {
    PolicyIr::Pgn {
        sources: sources.iter().map(|s| (*s).to_string()).collect(),
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: constraint.to_string(),
        on_fail,
        description: None,
    }
}

pub fn pgn_policy_sink(sources: &[&str], sink: &str, constraint: &str) -> PolicyIr {
    PolicyIr::Pgn {
        sources: sources.iter().map(|s| (*s).to_string()).collect(),
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: Some(sink.to_string()),
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: constraint.to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }
}

pub fn pgn_policy_required(sources: &[&str], required: &[&str], constraint: &str) -> PolicyIr {
    PolicyIr::Pgn {
        sources: sources.iter().map(|s| (*s).to_string()).collect(),
        required_sources: required.iter().map(|s| (*s).to_string()).collect(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: constraint.to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }
}

pub fn rewriter_with_policies(policies: &[PolicyIr]) -> PassantRewriter {
    let mut rewriter = PassantRewriter::new();
    for policy in policies {
        rewriter
            .register_policy(policy.clone())
            .expect("fixture must provide explicit SOURCE/SINK catalog metadata");
    }
    rewriter
}

pub fn rewrite(sql: &str, policies: &[PolicyIr]) -> String {
    rewrite_result(sql, policies).unwrap_or_else(|err| panic!("rewrite failed for {sql:?}: {err}"))
}

pub fn rewrite_result(sql: &str, policies: &[PolicyIr]) -> Result<String, RewriteError> {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policies(policies.to_vec())?;
    rewriter.rewrite(sql)
}

pub fn rewrite_with_catalog(sql: &str, policies: &[PolicyIr], catalog: TableCatalog) -> String {
    let mut rewriter = PassantRewriter::with_catalog(catalog);
    for policy in policies {
        rewriter
            .register_policy(policy.clone())
            .expect("fixture must provide explicit SOURCE/SINK catalog metadata");
    }
    rewriter
        .rewrite(sql)
        .unwrap_or_else(|err| panic!("rewrite failed for {sql:?}: {err}"))
}

pub fn assert_rewrite(sql: &str, policies: &[PolicyIr], expected: &str) {
    let actual = rewrite(sql, policies);
    pretty_assertions::assert_eq!(actual, expected);
}

pub fn assert_rewrite_fails_with(sql: &str, policies: &[PolicyIr], expected_substring: &str) {
    let err = rewrite_result(sql, policies).expect_err("rewrite should fail");
    let message = err.to_string();
    assert!(
        message.contains(expected_substring),
        "expected error containing {expected_substring:?}, got {message:?}"
    );
}

pub fn plan_query(sql: &str, policies: &[PolicyIr]) -> PlanQueryResult {
    let ir = parse_query_to_ir(sql).expect("query should parse");
    PassantPlanner::new().plan_query(&ir, policies)
}

pub fn assert_explain_strategy(sql: &str, policies: &[PolicyIr], strategy: RewriteStrategy) {
    assert_eq!(plan_query(sql, policies).chosen.strategy, strategy);
}

pub fn pgn_policy_kill(sources: &[&str], constraint: &str) -> PolicyIr {
    pgn_policy_with(sources, constraint, Resolution::Kill)
}

/// Caller-owned complete fixture schemas; never inferred from SQL or policies.
pub fn fixture_catalog(tables: &[(&str, &[&str])]) -> TableCatalog {
    let mut catalog = TableCatalog::new();
    for (table, columns) in tables {
        catalog.register_table(
            *table,
            columns.iter().map(|column| (*column).to_string()).collect(),
        );
    }
    catalog
}

pub fn rewriter_with_catalog(policies: &[PolicyIr], catalog: TableCatalog) -> PassantRewriter {
    PassantRewriter::from_policies_with_catalog(policies.to_vec(), catalog)
        .expect("policy should match caller-declared fixture schema")
}

pub fn assert_rewrite_with_catalog(
    sql: &str,
    policies: &[PolicyIr],
    expected: &str,
    catalog: TableCatalog,
) {
    pretty_assertions::assert_eq!(rewrite_with_catalog(sql, policies, catalog), expected);
}

pub fn plan_query_with_catalog(
    sql: &str,
    policies: &[PolicyIr],
    catalog: TableCatalog,
) -> PlanQueryResult {
    let ir = parse_query_to_ir(sql).expect("query should parse");
    let rewriter = rewriter_with_catalog(policies, catalog);
    PassantPlanner::new().plan_query_with_rewriter(&ir, &rewriter)
}

/// Compare public schema and bag semantics, not optimizer-specific SQL spelling.
pub fn assert_same_rows(actual: &str, reference: &str, setup: &str) {
    let db = duckdb::Connection::open_in_memory().expect("fixture database");
    db.execute_batch(setup)
        .expect("explicit fixture schema/data");
    let read = |sql: &str| {
        let mut stmt = db.prepare(sql).unwrap_or_else(|e| panic!("{e}: {sql}"));
        let mut cursor = stmt.query([]).expect("execute reference/rewritten SQL");
        let names = cursor.as_ref().expect("statement").column_names();
        let width = cursor.as_ref().expect("statement").column_count();
        let mut rows = Vec::new();
        while let Some(row) = cursor.next().expect("next row") {
            rows.push(
                (0..width)
                    .map(|i| row.get::<_, duckdb::types::Value>(i))
                    .collect::<duckdb::Result<Vec<_>>>()
                    .expect("row values"),
            );
        }
        let mut rows = rows
            .into_iter()
            .map(|row| format!("{row:?}"))
            .collect::<Vec<_>>();
        rows.sort();
        (names, rows)
    };
    assert_eq!(
        read(actual),
        read(reference),
        "rewritten: {actual}\nreference: {reference}"
    );
}
