use passant_core::{PassantRewriter, PolicyIr, Resolution};

fn remove_policy(source: &str, constraint: &str) -> PolicyIr {
    PolicyIr::Pgn {
        sources: vec![source.to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: constraint.to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }
}

#[test]
fn rewrite_with_large_registry_uses_scan_ready_without_reparsing() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(remove_policy("orders", "max(orders.amount) > 1"));
    for index in 0..5_000_usize {
        rewriter.register_policy(remove_policy(
            &format!("other_{index:05}"),
            &format!("max(other_{index}.amount) > {index}"),
        ));
    }

    let options = passant_core::RewriteOptions {
        collect_stats: true,
        ..passant_core::RewriteOptions::default()
    };
    rewriter
        .rewrite_with_options("SELECT id, amount FROM orders WHERE amount > 0", options)
        .expect("rewrite should succeed");

    assert_eq!(
        rewriter
            .last_rewrite_stats()
            .policy_constraints_parsed_during_rewrite,
        0
    );
}
