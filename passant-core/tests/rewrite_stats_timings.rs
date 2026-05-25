use passant_core::{PassantRewriter, PolicyIr, Resolution, RewriteOptions};

fn remove_policy(source: &str) -> PolicyIr {
    PolicyIr::CompatDfc {
        sources: vec![source.to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: format!("max({source}.amount) > 1"),
        on_fail: Resolution::Remove,
        description: None,
    }
}

#[test]
fn rewrite_stats_record_phase_timings() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(remove_policy("orders"));

    let options = RewriteOptions {
        collect_stats: true,
        ..RewriteOptions::default()
    };
    rewriter
        .rewrite_with_options("SELECT id, amount FROM orders WHERE amount > 0", options)
        .expect("rewrite should succeed");

    let stats = rewriter.last_rewrite_stats();
    let timings = stats.timings();
    assert!(timings.elapsed_parse_ms > 0.0);
    assert!(timings.elapsed_rewrite_ms > 0.0);
    assert!(timings.elapsed_total_ms > 0.0);
    assert_eq!(stats.policy_constraints_parsed_during_rewrite, 0);
    assert!(stats.query_nodes >= 1);
    assert!(stats.ast_nodes_visited_analysis > 0);
    assert!(stats.ast_nodes_visited_rewrite > 0);
}
