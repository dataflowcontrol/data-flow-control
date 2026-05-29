use passant_core::{PassantRewriter, PolicyIr, Resolution};

#[test]
fn rewriter_recurses_into_derived_subqueries() {
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
        .rewrite("SELECT id FROM (SELECT id FROM foo) AS q")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "SELECT id FROM (SELECT id FROM foo WHERE foo.id > 1) AS q"
    );
}

#[test]
fn rewriter_recurses_into_ctes() {
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
        .rewrite("WITH q AS (SELECT id FROM foo) SELECT id FROM q")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "WITH q AS (SELECT id FROM foo WHERE foo.id > 1) SELECT id FROM q"
    );
}

#[test]
fn rewriter_recurses_into_union_branches() {
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
        .rewrite("SELECT id FROM foo UNION ALL SELECT id FROM bar")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "SELECT id FROM foo WHERE foo.id > 1 UNION ALL SELECT id FROM bar"
    );
}

#[test]
fn rewriter_recurses_into_intersect_branches() {
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
        .rewrite("SELECT id FROM foo INTERSECT SELECT id FROM bar")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "SELECT id FROM foo WHERE foo.id > 1 INTERSECT SELECT id FROM bar"
    );
}

#[test]
fn rewriter_recurses_into_exists_subquery() {
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
        .rewrite("SELECT id FROM bar WHERE EXISTS (SELECT id FROM foo)")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "SELECT id FROM bar WHERE EXISTS (SELECT id FROM foo WHERE foo.id > 1)"
    );
}

#[test]
fn rewriter_recurses_into_in_subquery() {
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
        .rewrite("SELECT id FROM bar WHERE id IN (SELECT id FROM foo)")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "SELECT id FROM bar WHERE id IN (SELECT id FROM foo WHERE foo.id > 1)"
    );
}

#[test]
fn rewriter_recurses_into_not_exists_subquery() {
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
        .rewrite("SELECT id FROM bar WHERE NOT EXISTS (SELECT id FROM foo)")
        .expect("anti-semi subquery should rewrite");
    assert_eq!(
        sql,
        "SELECT id FROM bar WHERE NOT EXISTS (SELECT id FROM foo WHERE foo.id > 1)"
    );
}

#[test]
fn rewriter_recurses_into_not_in_subquery() {
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
        .rewrite("SELECT id FROM bar WHERE id NOT IN (SELECT id FROM foo)")
        .expect("anti-semi subquery should rewrite");
    assert_eq!(
        sql,
        "SELECT id FROM bar WHERE id NOT IN (SELECT id FROM foo WHERE foo.id > 1)"
    );
}
