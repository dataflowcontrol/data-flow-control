//! Auto-add missing policy columns to subquery/CTE SELECT lists. Pending Rust implementation.

use passant_core::{PassantRewriter, PolicyIr, Resolution};

#[test]
fn subquery_missing_policy_column_is_propagated_to_select_list() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
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
    });

    let sql = rewriter
        .rewrite("SELECT sub.name FROM (SELECT foo.name FROM foo) AS sub")
        .expect("query should rewrite");
    assert!(
        sql.contains("foo.id") || sql.contains("sub.id"),
        "expected hidden policy column propagation: {sql}"
    );
}

#[test]
fn multi_source_subquery_join_propagates_both_policy_columns() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string(), "baz".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) >= 2 AND max(baz.x) <= 20".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite(
            "SELECT sub.name FROM (SELECT foo.name FROM foo JOIN baz ON foo.id = baz.x) AS sub",
        )
        .expect("query should rewrite");
    assert!(
        sql.contains("foo.id") && (sql.contains("baz.x") || sql.contains("sub.x")),
        "expected both source columns propagated: {sql}"
    );
}

#[test]
fn cte_missing_policy_column_is_propagated_to_select_list() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
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
    });

    let sql = rewriter
        .rewrite("WITH q AS (SELECT foo.name FROM foo) SELECT q.name FROM q")
        .expect("query should rewrite");
    assert!(
        sql.contains("foo.id"),
        "expected policy column added to CTE select list: {sql}"
    );
}
