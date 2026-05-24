use crate::common::{assert_rewrite, dfc_policy, dfc_policy_invalidate, dfc_policy_kill, rewrite};

#[test]
fn existing_where_or_clause_combines_with_policy_using_and() {
    assert_rewrite(
        "SELECT id FROM foo WHERE id = 1 OR id = 3",
        &[dfc_policy(&["foo"], "max(foo.id) > 1")],
        "SELECT id FROM foo WHERE id = 1 OR id = 3 AND foo.id > 1",
    );
}

#[test]
fn existing_having_or_clause_combines_with_policy_using_and() {
    assert_rewrite(
        "SELECT category, SUM(amount) FROM foo GROUP BY category HAVING SUM(amount) > 0 OR SUM(amount) < 0",
        &[dfc_policy(&["foo"], "max(foo.amount) > 6")],
        "SELECT category, SUM(amount) FROM foo GROUP BY category HAVING SUM(amount) > 0 OR SUM(amount) < 0 AND max(foo.amount) > 6",
    );
}

#[test]
fn multiple_remove_policies_on_same_source_combine_with_and() {
    assert_rewrite(
        "SELECT id FROM foo",
        &[
            dfc_policy(&["foo"], "max(foo.id) > 1"),
            dfc_policy(&["foo"], "max(foo.amount) > 10"),
        ],
        "SELECT id FROM foo WHERE foo.id > 1 AND foo.amount > 10",
    );
}

#[test]
fn kill_and_invalidate_policies_combine_on_scan() {
    let sql = rewrite(
        "SELECT id FROM foo",
        &[
            dfc_policy_kill(&["foo"], "max(foo.id) > 10"),
            dfc_policy_invalidate(&["foo"], "max(foo.amount) > 0"),
        ],
    );
    assert!(sql.contains("valid"));
    assert!(sql.contains("kill()"));
}
