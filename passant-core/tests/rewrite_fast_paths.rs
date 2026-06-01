use passant_core::{CatalogSnapshot, PassantRewriter, PolicyIr, Resolution, TableCatalog};

fn remove_policy(source: &str) -> PolicyIr {
    PolicyIr::Pgn {
        sources: vec![source.to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: format!("max({source}.amount) > 0"),
        on_fail: Resolution::Remove,
        description: None,
    }
}

#[test]
fn rewrite_without_policies_returns_input_unchanged() {
    let rewriter = PassantRewriter::new();
    let sql = "SELECT id FROM orders";
    assert_eq!(rewriter.rewrite(sql).expect("rewrite"), sql);
}

#[test]
fn rewrite_with_unrelated_policies_returns_input_unchanged() {
    let mut rewriter = PassantRewriter::new();
    for index in 0..128 {
        rewriter.register_policy(remove_policy(&format!("other_{index:03}")));
    }
    let sql = "SELECT id, amount FROM orders";
    assert_eq!(rewriter.rewrite(sql).expect("rewrite"), sql);
}

#[test]
fn register_validated_policy_with_catalog_succeeds() {
    let mut catalog = TableCatalog::default();
    catalog.load_snapshot(CatalogSnapshot {
        tables: [(
            "orders".to_string(),
            passant_core::CatalogTableInfo {
                columns: vec!["id".to_string(), "amount".to_string()],
                types: Default::default(),
                row_count: None,
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    });
    let mut rewriter = PassantRewriter::with_catalog(catalog);
    rewriter
        .register_validated_policy(remove_policy("orders"))
        .expect("register should succeed");
    assert!(rewriter.has_registered_policies());
}
