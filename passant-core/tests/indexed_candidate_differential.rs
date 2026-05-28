use std::collections::HashSet;

use passant_core::{MultiSourceLookupMode, PolicyIr, PolicyStore, Resolution, TableKey};

fn dfc(source: &str, constraint: &str, on_fail: Resolution) -> PolicyIr {
    PolicyIr::Dfc {
        sources: vec![source.to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: constraint.to_string(),
        on_fail,
        description: None,
    }
}

fn register_diverse_registry(store: &mut PolicyStore) {
    store.register(dfc("orders", "max(orders.amount) > 1", Resolution::Remove));
    store.register(dfc("customers", "max(customers.id) > 0", Resolution::Kill));
    store.register(PolicyIr::Dfc {
        sources: vec!["orders".to_string(), "customers".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(orders.amount) + max(customers.id) > 10".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });
    store.register(PolicyIr::Dfc {
        sources: vec!["receipts".to_string()],
        required_sources: vec!["receipts".to_string()],
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "reports.id > 0".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });
    store.register(PolicyIr::Dfc {
        sources: vec![],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "max(reports.amount) <= 0".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });
    for index in 0..500_usize {
        store.register(dfc(
            &format!("other_{index:03}"),
            &format!("max(other_{index}.id) > {index}"),
            Resolution::Remove,
        ));
    }
}

#[test]
fn indexed_candidate_lookup_matches_slow_scan_for_diverse_registry() {
    let mut store = PolicyStore::default();
    register_diverse_registry(&mut store);

    let orders_only = HashSet::from([TableKey::new("orders")]);
    store.assert_candidates_match_slow_scan(&orders_only, None, MultiSourceLookupMode::Subset);
    assert_eq!(store.candidate_ids_for_scope(&orders_only, None), vec![0]);

    let join_scope = HashSet::from([TableKey::new("orders"), TableKey::new("customers")]);
    store.assert_candidates_match_slow_scan(&join_scope, None, MultiSourceLookupMode::Subset);
    assert_eq!(
        store.candidate_ids_for_scope(&join_scope, None),
        vec![0, 1, 2]
    );

    let empty_tables = HashSet::from([TableKey::new("unrelated")]);
    let sink = TableKey::new("reports");
    store.assert_candidates_match_slow_scan(
        &empty_tables,
        Some(&sink),
        MultiSourceLookupMode::Subset,
    );
}

#[test]
fn overlap_mode_includes_partially_visible_multi_source_policies() {
    let mut store = PolicyStore::default();
    register_diverse_registry(&mut store);

    let orders_only = HashSet::from([TableKey::new("orders")]);
    store.assert_candidates_match_slow_scan(&orders_only, None, MultiSourceLookupMode::AnyOverlap);
    assert_eq!(
        store.candidate_ids_for_scope_with_overlap(&orders_only, None),
        vec![0, 2]
    );
}
