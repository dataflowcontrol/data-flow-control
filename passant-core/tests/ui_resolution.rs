use passant_core::{PassantRewriter, PolicyIr, Resolution, RewriteOptions};

fn ui_insert_policy() -> PolicyIr {
    PolicyIr::Pgn {
        sources: vec!["bank_txn".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: Some("irs_form".to_string()),
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "NOT bank_txn.category = 'meal' OR irs_form.business_use_pct <= 50".to_string(),
        on_fail: Resolution::Ui,
        description: Some("Meal expenses require business use <= 50%.".to_string()),
    }
}

#[test]
fn ui_resolution_parses_and_labels() {
    assert_eq!(Resolution::parse("UI").unwrap(), Resolution::Ui);
    assert_eq!(Resolution::Ui.as_label(), "UI");
}

#[test]
fn ui_insert_rewrite_emits_filter_udf_not_tuple_ctes() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(ui_insert_policy());
    let options = RewriteOptions {
        ui_stream_endpoint: Some("/tmp/passant-ui-test.tsv".to_string()),
        ..RewriteOptions::default()
    };
    let sql = rewriter
        .rewrite_with_options(
            "INSERT INTO irs_form SELECT txn_id, amount, category, business_use_pct FROM bank_txn",
            options,
        )
        .expect("rewrite");
    assert!(sql.contains("address_violating_rows"));
    assert!(sql.contains("WHERE"));
    assert!(!sql.contains("t1 AS"));
    assert!(!sql.contains("t3 AS"));
    assert!(sql.contains("/tmp/passant-ui-test.tsv"));
    assert!(sql.contains("bank_txn.category"));
}

fn ui_select_policy() -> PolicyIr {
    PolicyIr::Pgn {
        sources: vec!["bank_txn".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "bank_txn.category = 'meal'".to_string(),
        on_fail: Resolution::Ui,
        description: None,
    }
}

#[test]
fn ui_standalone_select_emits_filter_udf() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(ui_select_policy());
    let options = RewriteOptions {
        ui_stream_endpoint: Some("/tmp/passant-ui-select.tsv".to_string()),
        ..RewriteOptions::default()
    };
    let sql = rewriter
        .rewrite_with_options("SELECT txn_id, amount, category FROM bank_txn", options)
        .expect("rewrite");
    assert!(sql.contains("address_violating_rows"));
    assert!(!sql.contains("t1 AS"));
}

fn ui_update_policy() -> PolicyIr {
    PolicyIr::Pgn {
        sources: vec![],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: Some("irs_form".to_string()),
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "irs_form.business_use_pct > 100".to_string(),
        on_fail: Resolution::Ui,
        description: None,
    }
}

#[test]
fn ui_update_approval_emits_passant_ui_approve() {
    let mut catalog = passant_core::TableCatalog::new();
    catalog.register_table("irs_form", vec!["txn_id".into(), "business_use_pct".into()]);
    let mut rewriter = PassantRewriter::with_catalog(catalog);
    rewriter.register_policy(ui_update_policy());
    let options = RewriteOptions {
        ui_update_mode: passant_core::UiUpdateMode::ApprovalOnly,
        ..RewriteOptions::default()
    };
    let sql = rewriter
        .rewrite_with_options(
            "UPDATE irs_form SET business_use_pct = 10 WHERE txn_id = 1",
            options,
        )
        .expect("rewrite");
    assert!(sql.contains("passant_ui_approve"));
}

#[test]
fn ui_update_edited_sets_followup_sql() {
    let mut catalog = passant_core::TableCatalog::new();
    catalog.register_unique_column("irs_form", "txn_id");
    catalog.register_table("irs_form", vec!["txn_id".into(), "business_use_pct".into()]);
    let mut rewriter = PassantRewriter::with_catalog(catalog);
    rewriter.register_policy(ui_update_policy());
    let options = RewriteOptions {
        ui_stream_endpoint: Some("/tmp/passant-ui-update.tsv".to_string()),
        ui_update_mode: passant_core::UiUpdateMode::EditedRows,
        ..RewriteOptions::default()
    };
    let sql = rewriter
        .rewrite_with_options(
            "UPDATE irs_form SET business_use_pct = 10 WHERE txn_id = 1",
            options,
        )
        .expect("rewrite");
    assert!(sql.contains("address_violating_rows"));
    let followup = rewriter.last_ui_followup_sql().expect("followup sql");
    assert!(followup.contains("read_csv"));
    assert!(followup.contains("irs_form.txn_id = staged.txn_id"));
    assert!(followup.contains("SET business_use_pct = staged.business_use_pct"));
    assert!(!followup.contains("irs_form.business_use_pct = staged"));
    assert!(followup.contains("names=['txn_id', 'business_use_pct']"));
}

#[test]
fn ui_partial_push_is_rejected() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: Some("bar".to_string()),
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Ui,
        description: None,
    });
    let options = RewriteOptions {
        use_partial_push: true,
        ..RewriteOptions::default()
    };
    let err = rewriter
        .rewrite_with_options("INSERT INTO bar SELECT id FROM foo", options)
        .expect_err("partial push with UI should fail");
    assert!(err.to_string().contains("partial-push"));
}
