use passant_core::{PassantRewriter, PolicyIr, Resolution};

#[test]
fn rewriter_expands_insert_columns_from_catalog_for_sink_policy() {
    let mut catalog = passant_core::TableCatalog::new();
    catalog.register_table("reports", vec!["id".into(), "status".into()]);
    let mut rewriter = PassantRewriter::with_catalog(catalog);
    rewriter.register_policy(PolicyIr::Dfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "reports.status = 'approved' AND max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("INSERT INTO reports SELECT foo.id, foo.status FROM foo")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo WHERE foo.status = 'approved' AND foo.id > 1"
    );
}

#[test]
fn rewriter_maps_insert_sink_columns_to_select_outputs() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Dfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "reports.status = 'approved' AND max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo WHERE foo.status = 'approved' AND foo.id > 1"
    );
}

#[test]
fn rewriter_maps_insert_sink_alias_columns_to_select_outputs() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Dfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: Some("r".to_string()),
        constraint: "r.status = 'approved' AND max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo WHERE foo.status = 'approved' AND foo.id > 1"
    );
}

#[test]
fn rewriter_maps_output_marker_columns_to_insert_outputs() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Dfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "_OUTPUT_.status = 'approved' AND max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo WHERE foo.status = 'approved' AND foo.id > 1"
    );
}

#[test]
fn rewriter_fails_closed_for_missing_required_source_on_insert() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Dfc {
        sources: vec!["receipts".to_string()],
        required_sources: vec!["receipts".to_string()],
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "reports.id > 0 AND max(receipts.id) > 0".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("INSERT INTO reports (id) SELECT other.id FROM other")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "INSERT INTO reports (id) SELECT other.id FROM other WHERE false"
    );
}

#[test]
fn rewriter_enforces_required_source_normally_when_present_on_insert() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Dfc {
        sources: vec!["receipts".to_string()],
        required_sources: vec!["receipts".to_string()],
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "reports.id > 0 AND max(receipts.id) > 10".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("INSERT INTO reports (id) SELECT receipts.id FROM receipts")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "INSERT INTO reports (id) SELECT receipts.id FROM receipts WHERE receipts.id > 0 AND receipts.id > 10"
    );
}
