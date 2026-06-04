use passant_core::{PassantRewriter, PolicyIr, Resolution, TableCatalog};

fn catalog_with_reports() -> TableCatalog {
    let mut catalog = TableCatalog::new();
    catalog.register_table(
        "reports".to_string(),
        vec!["id".to_string(), "status".to_string()],
    );
    catalog
}

#[test]
fn rewriter_maps_output_marker_in_select_policy() {
    let mut rewriter = PassantRewriter::with_catalog(TableCatalog::new());
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "_OUTPUT_.status = 'approved' AND max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT foo.id, foo.status FROM foo")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "SELECT foo.id, foo.status FROM foo WHERE foo.status = 'approved' AND foo.id > 1"
    );
}

#[test]
fn rewriter_maps_output_marker_in_update_for_assigned_and_unassigned_columns() {
    let mut rewriter = PassantRewriter::with_catalog(catalog_with_reports());
    rewriter.register_policy(PolicyIr::Pgn {
        sources: Vec::new(),
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "_OUTPUT_.status = 'approved' AND _OUTPUT_.id > 0".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("UPDATE reports SET status = 'approved'")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "UPDATE reports SET status = 'approved' WHERE 'approved' = 'approved' AND reports.id > 0"
    );
}

#[test]
fn rewriter_rejects_ambiguous_output_marker_reference() {
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
        constraint: "_OUTPUT_.id > 1 AND max(foo.id) > 0".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let err = rewriter
        .rewrite("SELECT a.id, b.id FROM foo AS a JOIN foo AS b ON a.id = b.id")
        .expect_err("ambiguous _OUTPUT_ should fail");
    assert!(err.to_string().contains("ambiguous _OUTPUT_ column 'id'"));
}
