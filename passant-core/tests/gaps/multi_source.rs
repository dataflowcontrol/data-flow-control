use crate::common::{assert_rewrite, dfc_policy, rewrite};

fn multi_source_policy() -> passant_core::PolicyIr {
    dfc_policy(&["foo", "baz"], "max(foo.id) >= 2 AND max(baz.x) <= 20")
}

#[test]
fn multi_source_policy_requires_all_sources_before_applying() {
    assert_rewrite(
        "SELECT MAX(foo.id) FROM foo",
        &[multi_source_policy()],
        "SELECT MAX(foo.id) FROM foo",
    );
}

#[test]
fn multi_source_aggregation_with_inner_join_pushes_predicates_into_join() {
    assert_rewrite(
        "SELECT MAX(foo.id), MAX(baz.x) FROM foo JOIN baz ON foo.id = baz.x",
        &[multi_source_policy()],
        "SELECT MAX(foo.id), MAX(baz.x) FROM foo JOIN baz ON foo.id = baz.x AND foo.id >= 2 AND baz.x <= 20",
    );
}

#[test]
fn multi_source_scan_with_left_join_splits_predicates_across_join_and_where() {
    let sql = rewrite(
        "SELECT foo.id, baz.x FROM foo LEFT JOIN baz ON foo.id = baz.x",
        &[multi_source_policy()],
    );
    assert!(sql.contains("baz.x <= 20"));
    assert!(sql.contains("foo.id >= 2"));
    assert!(!sql.contains("WITH base_query"));
}

#[test]
fn multi_source_group_by_with_extra_join_pushes_predicates_into_join() {
    assert_rewrite(
        "SELECT foo.name, MAX(baz.x) FROM foo JOIN baz ON foo.id = baz.x JOIN qux ON TRUE GROUP BY foo.name",
        &[multi_source_policy()],
        "SELECT foo.name, MAX(baz.x) FROM foo JOIN baz ON foo.id = baz.x JOIN qux ON true AND foo.id >= 2 AND baz.x <= 20 GROUP BY foo.name",
    );
}

#[test]
fn multi_source_distinct_scan_pushes_predicates_into_join() {
    assert_rewrite(
        "SELECT DISTINCT foo.id, baz.x FROM foo JOIN baz ON foo.id = baz.x",
        &[multi_source_policy()],
        "SELECT DISTINCT foo.id, baz.x FROM foo JOIN baz ON foo.id = baz.x AND foo.id >= 2 AND baz.x <= 20",
    );
}

#[test]
fn multi_source_having_with_existing_having_pushes_predicates_into_join() {
    assert_rewrite(
        "SELECT foo.id, MAX(baz.x) FROM foo JOIN baz ON foo.id = baz.x GROUP BY foo.id HAVING MAX(baz.x) > 0",
        &[multi_source_policy()],
        "SELECT foo.id, MAX(baz.x) FROM foo JOIN baz ON foo.id = baz.x AND foo.id >= 2 AND baz.x <= 20 GROUP BY foo.id HAVING MAX(baz.x) > 0",
    );
}
