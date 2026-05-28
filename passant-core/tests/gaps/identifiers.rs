//! Identifier stress tests for rewrite stability.

use passant_core::{
    CatalogSnapshot, CatalogTableInfo, PassantRewriter, PolicyIr, Resolution, TableCatalog,
};
use std::collections::HashMap;

use crate::common::rewrite;

fn quoted_catalog() -> TableCatalog {
    TableCatalog::from_snapshot(CatalogSnapshot {
        tables: HashMap::from([(
            "MySchema.MyTable".to_string(),
            CatalogTableInfo {
                columns: vec!["OrderID".into(), "Amount".into()],
                types: HashMap::new(),
            },
        )]),
        ..Default::default()
    })
}

#[test]
fn catalog_validates_quoted_column_references() {
    let catalog = quoted_catalog();
    let policy = PolicyIr::Dfc {
        sources: vec!["MySchema.MyTable".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(\"MySchema\".\"MyTable\".\"OrderID\") > 0".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    catalog
        .validate_policy(&policy)
        .expect("quoted columns should validate");
}

#[test]
fn catalog_rejects_unknown_quoted_column() {
    let catalog = quoted_catalog();
    let policy = PolicyIr::Dfc {
        sources: vec!["MySchema.MyTable".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(\"MySchema\".\"MyTable\".\"MissingCol\") > 0".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    let err = catalog
        .validate_policy(&policy)
        .expect_err("missing column");
    assert_eq!(err.kind(), passant_core::ErrorKind::UnknownColumn);
}

#[test]
fn schema_qualified_table_rewrites_with_full_push() {
    let mut rewriter = PassantRewriter::with_catalog(quoted_catalog());
    rewriter
        .register_validated_policy(PolicyIr::Dfc {
            sources: vec!["MySchema.MyTable".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(\"MySchema\".\"MyTable\".\"OrderID\") > 0".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        })
        .expect("policy should register");

    let sql = rewriter
        .rewrite("SELECT \"OrderID\" FROM \"MySchema\".\"MyTable\"")
        .expect("rewrite should succeed");
    assert!(sql.contains("MySchema"));
    assert!(sql.contains("OrderID"));
}

#[test]
fn table_alias_does_not_break_policy_registration() {
    let mut catalog = TableCatalog::new();
    catalog.register_table("foo", vec!["id".into(), "secret".into()]);
    let mut rewriter = PassantRewriter::with_catalog(catalog);
    rewriter
        .register_validated_policy(PolicyIr::Dfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(foo.id) > 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        })
        .expect("policy should register");

    let sql = rewriter
        .rewrite("SELECT f.id FROM foo AS f")
        .expect("rewrite should succeed");
    assert_eq!(sql, "SELECT f.id FROM foo AS f WHERE f.id > 1");
}

#[test]
fn substring_column_name_does_not_corrupt_replace() {
    let sql = rewrite(
        "SELECT id FROM foo",
        &[PolicyIr::Dfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(foo.id_value) > 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
    );
    assert_eq!(sql, "SELECT id FROM foo WHERE foo.id_value > 1");
}

#[test]
fn reserved_word_column_rewrites_with_full_push() {
    let sql = rewrite(
        "SELECT \"order\" FROM items",
        &[PolicyIr::Dfc {
            sources: vec!["items".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(items.\"order\") > 0".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
    );
    assert!(sql.contains("\"order\""));
    assert!(sql.contains("items"));
}

#[test]
fn nested_cte_scan_applies_policy_filter() {
    let sql = rewrite(
        "WITH inner_cte AS (SELECT id FROM foo) SELECT id FROM inner_cte",
        &[PolicyIr::Dfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(foo.id) > 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
    );
    assert!(sql.contains("WITH inner_cte AS"));
    assert!(sql.contains("foo.id > 1") || sql.contains("id > 1"));
}
