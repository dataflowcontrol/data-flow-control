//! Non-distributive scan predicates should fuse same-source comparisons into one subquery.

use crate::common::{pgn_policy, rewrite};

#[test]
fn same_source_non_distributive_comparison_uses_single_subquery_scan() {
    let sql = rewrite(
        "SELECT id FROM foo",
        &[pgn_policy(
            &["foo"],
            "count(DISTINCT foo.id) > count(DISTINCT foo.amount)",
        )],
    );
    let subquery_count = sql.matches("SELECT count").count();
    assert!(
        subquery_count <= 1,
        "expected at most one count subquery scan over foo, got {subquery_count} in: {sql}"
    );
}
