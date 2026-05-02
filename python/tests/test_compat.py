from passant.compat import AggregateDFCPolicy, DFCPolicy, Resolution, SQLRewriter


def test_python_compat_rewriter_preserves_policy_registration():
    rewriter = SQLRewriter()
    policy = DFCPolicy(
        sources=["foo"],
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    rewriter.register_policy(policy)
    assert rewriter.get_dfc_policies() == [policy]


def test_python_compat_transform_query_falls_back_without_extension():
    rewriter = SQLRewriter()
    query = "SELECT id FROM foo"
    assert isinstance(rewriter.transform_query(query), str)


def test_python_compat_execute_round_trips_through_duckdb():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    assert rewriter.fetchall("SELECT id FROM foo ORDER BY id") == [(1,), (2,)]


def test_python_compat_finalize_aggregate_policies_returns_mapping():
    rewriter = SQLRewriter()
    rewriter.register_policy(
        AggregateDFCPolicy(
            sources=["foo"],
            sink="reports",
            constraint="sum(reports.id) > 1",
            on_fail=Resolution.INVALIDATE,
        )
    )
    assert rewriter.finalize_aggregate_policies("reports") == {
        "aggregate::sum(reports.id) > 1": None
    }
