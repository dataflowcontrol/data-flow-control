use passant_core::{PassantRewriter, PolicyIr, Resolution, RewriteOptions};

fn cross_source_policy(left: &str, right: &str, constraint: &str) -> PolicyIr {
    PolicyIr::CompatDfc {
        sources: vec![left.to_string(), right.to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: constraint.to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }
}

#[test]
fn union_all_split_rewrite_uses_compiled_constraints_without_reparsing() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(cross_source_policy(
        "foo",
        "bar",
        "max(foo.id) > 1 AND max(bar.id) > 10",
    ));
    for index in 0..2_000_usize {
        rewriter.register_policy(PolicyIr::CompatDfc {
            sources: vec![format!("other_{index:05}")],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: format!("max(other_{index}.id) > {index}"),
            on_fail: Resolution::Remove,
            description: None,
        });
    }

    let options = RewriteOptions {
        collect_stats: true,
        ..RewriteOptions::default()
    };
    rewriter
        .rewrite_with_options(
            "SELECT foo.id FROM foo UNION ALL SELECT bar.id FROM bar",
            options,
        )
        .expect("union split rewrite should succeed");

    assert_eq!(
        rewriter
            .last_rewrite_stats()
            .policy_constraints_parsed_during_rewrite,
        0
    );
}
