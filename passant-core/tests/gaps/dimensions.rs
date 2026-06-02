//! DIMENSION relation injection and catalog validation.

use std::collections::HashMap;

use passant_core::{PolicyIr, Resolution, TableCatalog};

use crate::common::{assert_rewrite, rewrite_with_catalog};

fn dimension_policy(
    sources: &[&str],
    tables: &[&str],
    aliases: &[(&str, &str)],
    constraint: &str,
) -> PolicyIr {
    PolicyIr::Pgn {
        sources: sources.iter().map(|s| (*s).to_string()).collect(),
        required_sources: Vec::new(),
        dimension_tables: tables.iter().map(|t| (*t).to_string()).collect(),
        dimension_aliases: aliases
            .iter()
            .map(|(alias, base)| (alias.to_string(), base.to_string()))
            .collect(),
        dimension_queries: HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: HashMap::new(),
        constraint: constraint.to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }
}

#[test]
fn injects_singleton_dimension_via_cross_join() {
    let mut catalog = TableCatalog::new();
    catalog.register_table("foo", vec!["id".to_string(), "region_id".to_string()]);
    catalog.register_table("session_user", vec!["user_id".to_string()]);
    catalog.register_table_row_count("session_user", 1);
    let policy = dimension_policy(
        &["foo"],
        &["session_user"],
        &[("u", "session_user")],
        "max(foo.id) > 0 AND u.user_id = 1",
    );
    let sql = rewrite_with_catalog("SELECT foo.id FROM foo", &[policy], catalog);
    assert!(
        sql.contains("CROSS JOIN") && sql.contains("session_user"),
        "expected singleton dimension cross join: {sql}"
    );
    assert!(
        sql.contains("u.user_id = 1"),
        "expected dimension predicate: {sql}"
    );
}

#[test]
fn skipped_dimension_with_predicate_fails_closed() {
    let mut catalog = TableCatalog::new();
    catalog.register_table("foo", vec!["id".to_string()]);
    catalog.register_table("regions", vec!["id".to_string(), "code".to_string()]);
    catalog.register_table_row_count("regions", 2);
    let policy = dimension_policy(
        &["foo"],
        &["regions"],
        &[],
        "max(foo.id) > 0 AND regions.code = 'US'",
    );
    let sql = rewrite_with_catalog("SELECT foo.id FROM foo", &[policy], catalog);
    assert!(
        sql.contains("WHERE false") || sql.contains("where false"),
        "unjoined dimension predicate should fail closed: {sql}"
    );
    assert!(
        !sql.contains("regions.code"),
        "dimension columns should not appear when dimension was not joined: {sql}"
    );
}

#[test]
fn skips_dimension_without_join_key_or_singleton() {
    let mut catalog = TableCatalog::new();
    catalog.register_table("foo", vec!["id".to_string(), "region_id".to_string()]);
    catalog.register_table("regions", vec!["id".to_string(), "code".to_string()]);
    catalog.register_table_row_count("regions", 2);
    let policy = dimension_policy(
        &["foo"],
        &["regions"],
        &[],
        "max(foo.id) > 0 AND regions.code = 'US'",
    );
    let sql = rewrite_with_catalog("SELECT foo.id FROM foo", &[policy], catalog);
    assert!(
        !sql.contains("JOIN regions") && !sql.contains("CROSS JOIN regions"),
        "unsafe dimension should not be joined into FROM: {sql}"
    );
}

#[test]
fn dimension_already_in_query_is_not_duplicated() {
    assert_rewrite(
        "SELECT foo.id FROM foo JOIN regions ON foo.region_id = regions.id",
        &[dimension_policy(
            &["foo"],
            &["regions"],
            &[],
            "max(foo.id) > 1 AND regions.code = 'US'",
        )],
        "SELECT foo.id FROM foo JOIN regions ON foo.region_id = regions.id WHERE foo.id > 1 AND regions.code = 'US'",
    );
}

#[test]
fn dimension_alias_qualifiers_rewrite_after_injection() {
    let mut catalog = TableCatalog::new();
    catalog.register_table("foo", vec!["id".to_string()]);
    catalog.register_table("session_user", vec!["id".to_string(), "name".to_string()]);
    catalog.register_table_row_count("session_user", 1);
    let policy = dimension_policy(
        &["foo"],
        &["session_user"],
        &[("u", "session_user")],
        "max(foo.id) > 0 AND u.id = 1",
    );
    let sql = rewrite_with_catalog("SELECT foo.id FROM foo", &[policy], catalog);
    assert!(
        sql.contains("session_user") && sql.contains("u"),
        "expected aliased dimension in FROM: {sql}"
    );
    assert!(
        sql.contains("u.id = 1"),
        "expected dimension filter to keep policy alias: {sql}"
    );
}

#[test]
fn catalog_rejects_unknown_dimension_table() {
    let mut catalog = TableCatalog::new();
    catalog.register_table("foo", vec!["id".to_string()]);
    catalog.load_snapshot(passant_core::CatalogSnapshot {
        tables: [(
            "foo".to_string(),
            passant_core::CatalogTableInfo {
                columns: vec!["id".to_string()],
                types: std::collections::HashMap::new(),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    });
    let policy = dimension_policy(&["foo"], &["missing_dim"], &[], "max(foo.id) > 0");
    let err = catalog
        .validate_policy(
            &policy,
            &passant_core::AggregateRegistry::for_dialect(passant_core::SqlDialect::DuckDb),
        )
        .expect_err("missing dimension table");
    assert!(err.to_string().contains("Dimension table 'missing_dim'"));
}
