use passant_core::{CatalogSnapshot, PassantRewriter};

fn planner() -> PassantRewriter {
    let mut planner = PassantRewriter::new();
    planner.apply_catalog_snapshot(
        serde_json::from_value::<CatalogSnapshot>(serde_json::json!({
            "tables": {"source": {"columns": ["id", "amount"]}, "saved": {"columns": ["id"]}},
            "intermediate_bindings": {
                "source": {"source": "source"},
                "saved": {"state_column": "_dfc_intermediate_state"}
            }
        }))
        .unwrap(),
    );
    planner
}

#[test]
fn cte_shadow_does_not_export_a_fresh_source() {
    let sql = planner()
        .rewrite_intermediate_query("WITH source AS (SELECT id FROM saved) SELECT * FROM source")
        .unwrap();
    assert!(!sql.contains("source:source"));
    assert!(sql.contains("_dfc_intermediate_state"));
    assert!(!sql.contains("SELECT *"));
}

#[test]
fn frames_reuse_numbered_parameters() {
    let sql = planner().rewrite_intermediate_query(
        "SELECT id, SUM(amount) OVER (ORDER BY id ROWS BETWEEN ? PRECEDING AND CURRENT ROW) AS total FROM source WHERE amount > ?"
    ).unwrap();
    assert_eq!(sql.matches("$1").count(), 2);
    assert_eq!(sql.matches("$2").count(), 1);
    assert!(!sql.contains('?'));
}

#[test]
fn unsupported_windows_and_expression_subqueries_are_rejected() {
    for sql in [
        "SELECT LAG(id) OVER () FROM source",
        "SELECT id FROM source WHERE EXISTS (SELECT 1)",
        "SELECT id FROM source ORDER BY 3",
        "SELECT * FROM source LEFT JOIN saved ON source.id=saved.id",
    ] {
        assert!(planner().rewrite_intermediate_query(sql).is_err(), "{sql}");
    }
}
