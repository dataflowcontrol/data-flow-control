#[path = "common/mod.rs"]
mod common;

use passant_core::{PassantRewriter, PolicyIr, Resolution, RewriteOptions};

fn cross_source_policy(left: &str, right: &str, constraint: &str) -> PolicyIr {
    PolicyIr::Pgn {
        sources: vec![left.to_string(), right.to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: constraint.to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }
}

#[test]
fn union_all_split_rewrite_uses_compiled_constraints_without_reparsing() {
    let mut rewriter = PassantRewriter::with_catalog(common::fixture_catalog(&[
        ("foo", &["id"]),
        ("bar", &["id"]),
    ]));
    rewriter
        .register_policy(cross_source_policy(
            "foo",
            "bar",
            "max(foo.id) > 1 AND max(bar.id) > 10",
        ))
        .expect("generated fixture policy must validate");
    for index in 0..2_000_usize {
        rewriter
            .catalog_mut()
            .register_table(format!("other_{index:05}"), vec!["id".into()]);
        rewriter
            .register_policy(PolicyIr::Pgn {
                sources: vec![format!("other_{index:05}")],
                required_sources: Vec::new(),
                dimension_tables: Vec::new(),
                dimension_aliases: std::collections::HashMap::new(),
                dimension_queries: std::collections::HashMap::new(),
                sink: None,
                sink_alias: None,
                source_aliases: std::collections::HashMap::new(),
                constraint: format!("max(other_{index:05}.id) > {index}"),
                on_fail: Resolution::Remove,
                description: None,
            })
            .expect("generated fixture policy must validate");
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
