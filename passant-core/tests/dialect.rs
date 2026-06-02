use passant_core::{
    CatalogSnapshot, PassantRewriter, PolicyIr, Resolution, RewriteOptions, SqlDialect,
    parse_query_with_dialect,
};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

fn remove_scan_policy() -> PolicyIr {
    PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }
}

fn catalog_for_dialect(dialect: &str) -> CatalogSnapshot {
    CatalogSnapshot {
        dialect: Some(dialect.to_string()),
        default_schema: None,
        search_path: Vec::new(),
        tables: HashMap::new(),
        unique_columns: vec![],
        aggregate_functions: vec![],
    }
}

fn expected_sql(dialect: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("expected")
        .join(dialect)
        .join("remove_scan.sql");
    let display = path.display().to_string();
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read {display}: {err}"))
        .trim_end()
        .to_string()
}

#[test]
fn catalog_snapshot_dialect_is_applied_to_rewriter() {
    let mut rewriter = PassantRewriter::new();
    rewriter.apply_catalog_snapshot(catalog_for_dialect("sqlite"));
    assert_eq!(rewriter.sql_dialect(), SqlDialect::SQLite);
}

#[test]
fn parse_query_with_dialect_accepts_common_select() {
    let sql = "SELECT 1";
    assert!(parse_query_with_dialect(sql, SqlDialect::SQLite).is_ok());
    assert!(parse_query_with_dialect(sql, SqlDialect::Postgres).is_ok());
    assert!(parse_query_with_dialect(sql, SqlDialect::DuckDb).is_ok());
}

#[test]
fn remove_scan_sql_matches_expected_per_dialect() {
    for dialect in ["duckdb", "sqlite", "postgres"] {
        let mut rewriter = PassantRewriter::new();
        rewriter.apply_catalog_snapshot(catalog_for_dialect(dialect));
        rewriter.register_policy(remove_scan_policy());
        let rewritten = rewriter
            .rewrite("SELECT id FROM foo")
            .unwrap_or_else(|err| panic!("rewrite failed for {dialect}: {err}"));
        assert_eq!(rewritten, expected_sql(dialect));
    }
}

#[test]
fn rewrite_options_parse_dialect_overrides_catalog() {
    let mut rewriter = PassantRewriter::new();
    rewriter.apply_catalog_snapshot(catalog_for_dialect("duckdb"));
    rewriter.register_policy(remove_scan_policy());
    let options = RewriteOptions {
        parse_dialect: Some(SqlDialect::SQLite),
        ..RewriteOptions::default()
    };
    let rewritten = rewriter
        .rewrite_with_options("SELECT id FROM foo", options)
        .expect("rewrite with sqlite override");
    assert_eq!(rewritten, expected_sql("sqlite"));
}

#[test]
fn duckdb_specific_syntax_fails_under_sqlite_dialect() {
    let err = parse_query_with_dialect("SELECT {'a': 1}", SqlDialect::SQLite);
    assert!(
        err.is_err(),
        "expected sqlite parser to reject duckdb struct literal syntax"
    );
}
