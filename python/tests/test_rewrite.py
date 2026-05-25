"""End-to-end Python API tests: policy registration, rewrite, and DuckDB execution."""

from passant import AggregatePolicy, Policy, Resolution, wrap
import json
import duckdb
import pytest


def test_rewriter_preserves_policy_registration():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    policy = Policy(
        sources=["foo"],
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    rewriter.register_policy(policy)
    assert rewriter.policies() == [policy]
    assert json.loads(rewriter.planner.inner.dfc_policies_json())[0]["CompatDfc"]["sources"] == [
        "foo"
    ]


def test_delete_policy_updates_rust_storage():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1)")
    policy = Policy(
        sources=["foo"],
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    rewriter.register_policy(policy)

    assert rewriter.delete_policy(
        sources=["foo"],
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    assert rewriter.policies() == []
    assert rewriter.fetchall("SELECT id FROM foo") == [(1,)]


def test_get_pgn_policies_roundtrip():
    from passant import PgnPolicy

    rewriter = wrap(duckdb.connect())
    text = (
        "PGN OVER SOURCE foo SINK reports AGGREGATE sum(foo.amount) "
        "CONSTRAINT sum(foo.amount) <= 1000 ON FAIL REMOVE"
    )
    rewriter.register_policy(PgnPolicy.from_text(text))
    assert rewriter.pgn_policies() == [PgnPolicy(text=text)]
    assert rewriter.planner.inner.has_registered_policies()
    assert rewriter.policies() == []


def test_dfc_policy_from_policy_str_uses_rust_parser():
    policy = Policy.from_policy_str(
        "SOURCE foo SINK reports CONSTRAINT max(foo.id) > 1 ON FAIL KILL DESCRIPTION stop bad rows"
    )
    assert policy == Policy(
        sources=["foo"],
        sink="reports",
        constraint="max(foo.id) > 1",
        on_fail=Resolution.KILL,
        description="stop bad rows",
    )


def test_dfc_policy_from_policy_str_preserves_required_sources():
    policy = Policy.from_policy_str(
        "SOURCE REQUIRED receipts SINK reports CONSTRAINT reports.id > 0 ON FAIL REMOVE"
    )
    assert policy == Policy(
        sources=["receipts"],
        required_sources=["receipts"],
        sink="reports",
        constraint="reports.id > 0",
        on_fail=Resolution.REMOVE,
    )


def test_dfc_policy_from_policy_str_preserves_sink_alias():
    policy = Policy.from_policy_str(
        "SOURCE foo SINK reports AS r CONSTRAINT r.status = 'approved' ON FAIL REMOVE"
    )
    assert policy == Policy(
        sources=["foo"],
        sink="reports",
        sink_alias="r",
        constraint="r.status = 'approved'",
        on_fail=Resolution.REMOVE,
    )


def test_dfc_policy_from_policy_str_normalizes_source_alias():
    policy = Policy.from_policy_str(
        "SOURCE foo AS f SINK reports CONSTRAINT max(f.id) > 1 ON FAIL REMOVE"
    )
    assert policy == Policy(
        sources=["foo"],
        sink="reports",
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )


def test_dfc_policy_from_policy_str_preserves_dimensions():
    policy = Policy.from_policy_str(
        "SOURCE foo AS f SINK reports DIMENSION f.region, reports.department "
        "CONSTRAINT max(f.id) > 1 ON FAIL REMOVE"
    )
    assert policy == Policy(
        sources=["foo"],
        sink="reports",
        dimensions=["foo.region", "reports.department"],
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )


def test_aggregate_policy_from_policy_str_uses_rust_parser():
    policy = AggregatePolicy.from_policy_str(
        "AGGREGATE SOURCES foo SINK reports DIMENSION reports.region "
        "CONSTRAINT sum(reports.id) > 1 ON FAIL REMOVE"
    )
    assert policy == AggregatePolicy(
        sources=["foo"],
        sink="reports",
        dimensions=["reports.region"],
        constraint="sum(reports.id) > 1",
        on_fail=Resolution.REMOVE,
    )


def test_policy_requires_sources_list():
    with pytest.raises(ValueError, match="Sources must be provided"):
        Policy(
            sources=None,
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )


def test_policy_validates_constraint_syntax_at_construction():
    with pytest.raises(ValueError, match="Invalid constraint SQL expression"):
        Policy(
            sources=["foo"],
            constraint="max(foo.id) >",
            on_fail=Resolution.REMOVE,
        )


def test_policy_requires_qualified_constraint_columns_at_construction():
    with pytest.raises(ValueError, match="Unqualified columns found: id"):
        Policy(
            sources=["foo"],
            constraint="max(id) > 1",
            on_fail=Resolution.REMOVE,
        )


def test_transform_query_enforces_registered_policy():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    policy = Policy(
        sources=["foo"],
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    rewriter.register_policy(policy)
    assert rewriter.transform_query("SELECT id FROM foo") == "SELECT id FROM foo WHERE foo.id > 1"


def test_transform_query_collapses_dominated_thresholds():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 10",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.transform_query("SELECT id FROM foo") == "SELECT id FROM foo WHERE foo.id > 10"


def test_explain_rewrite_uses_registered_policies():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    explanation = rewriter.explain_rewrite("SELECT id FROM foo")
    assert explanation["chosen"]["rewritten_sql"] == "SELECT id FROM foo WHERE foo.id > 1"
    assert explanation["applicable_policies"][0]["CompatDfc"]["sources"] == ["foo"]


def test_explain_rewrite_reports_full_push_strategy_for_join():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    explanation = rewriter.explain_rewrite("SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id")
    assert explanation["chosen"]["strategy"] == "FullPush"
    assert explanation["scope"]["visible_tables"] == ["foo", "bar"]


def test_explain_rewrite_reports_non_distributive_policy_aggregate():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="avg(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    explanation = rewriter.explain_rewrite("SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id")
    assert explanation["chosen"]["strategy"] == "PartialPush"
    assert explanation["scope"]["policy_aggregate_count"] == 1
    assert explanation["scope"]["policy_aggregates_distributive"] is False
    assert explanation["scope"]["non_distributive_policy_aggregates"] == ["avg(foo.id)"]


def test_non_distributive_scan_policy_uses_partial_push():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (3)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="avg(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    transformed = rewriter.transform_query("SELECT id FROM foo")
    assert transformed.startswith("WITH base_query AS (")
    assert "policy_eval AS (" in transformed
    assert "CROSS JOIN policy_eval" in transformed
    assert rewriter.fetchall("SELECT id FROM foo ORDER BY id") == [(1,), (3,)]


def test_non_distributive_partial_push_handles_aliases():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (3)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="avg(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    transformed = rewriter.transform_query("SELECT f.id FROM foo AS f")
    assert transformed.startswith("WITH base_query AS (")
    assert "policy_eval AS (" in transformed
    assert "avg(f.id) > 1" in transformed
    assert rewriter.fetchall("SELECT f.id FROM foo AS f ORDER BY f.id") == [(1,), (3,)]


def test_non_distributive_scalar_fallback_splits_source_local_predicates():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (3)")
    rewriter.execute("INSERT INTO bar VALUES (20), (30)")
    rewriter.register_policy(
        Policy(
            sources=["foo", "bar"],
            constraint="avg(foo.id) > 1 AND avg(bar.id) > 10",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT foo.id FROM foo JOIN bar ON foo.id < bar.id ORDER BY foo.id"
    ) == [(1,), (1,), (3,), (3,)]


def test_cross_source_non_distributive_aggregate_comparison_uses_partial_push():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (3)")
    rewriter.execute("INSERT INTO bar VALUES (2), (4)")
    rewriter.register_policy(
        Policy(
            sources=["foo", "bar"],
            constraint="avg(foo.id) > avg(bar.id)",
            on_fail=Resolution.REMOVE,
        )
    )

    transformed = rewriter.transform_query("SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id")
    assert transformed.startswith("WITH base_query AS (")
    assert "avg(foo.id) > avg(bar.id)" in transformed
    assert rewriter.fetchall("SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id") == []


def test_rejects_mixed_row_and_non_distributive_aggregate_policy():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    with pytest.raises(ValueError, match="must be aggregated"):
        rewriter.register_policy(
            Policy(
                sources=["foo"],
                constraint="foo.id > 0 AND avg(foo.id) > 1",
                on_fail=Resolution.REMOVE,
            )
        )


def test_explain_rewrite_reports_unsupported_rewrite_error():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    explanation = rewriter.explain_rewrite("SELECT id FROM bar EXCEPT SELECT id FROM foo")
    assert explanation["scope"]["requires_source_set_annotations"] is True
    assert explanation["chosen"]["rewrite_error"] is None
    assert explanation["chosen"]["rewritten_sql"] == (
        "SELECT id FROM bar EXCEPT SELECT id FROM foo WHERE foo.id > 1"
    )


def test_explain_rewrite_reports_source_set_error():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["bar", "foo"],
            constraint="max(bar.id) > max(foo.id)",
            on_fail=Resolution.REMOVE,
        )
    )

    explanation = rewriter.explain_rewrite(
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id"
    )
    assert explanation["scope"]["requires_source_set_annotations"] is True
    assert explanation["chosen"]["rewrite_error"] is None
    rewritten = explanation["chosen"]["rewritten_sql"]
    assert "base_query" not in rewritten.lower()
    assert "bar.id > foo.id" in rewritten


def test_execute_round_trips_through_duckdb():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    assert rewriter.fetchall("SELECT id FROM foo ORDER BY id") == [(1,), (2,)]


def test_kill_resolution_aborts_query():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 10",
            on_fail=Resolution.KILL,
        )
    )

    with pytest.raises(Exception, match="KILLing due to dfc policy violation"):
        rewriter.fetchall("SELECT id FROM foo")


def test_insert_sink_policy_maps_output_columns():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER, status VARCHAR)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'draft'), (2, 'approved')")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            sink="reports",
            constraint="reports.status = 'approved' AND max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo")
    assert rewriter.fetchall("SELECT * FROM reports ORDER BY id") == [(2, "approved")]


def test_insert_without_column_list_maps_sink_columns_from_catalog():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER, status VARCHAR)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'draft'), (2, 'approved')")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            sink="reports",
            constraint="reports.status = 'approved' AND max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("INSERT INTO reports SELECT foo.id, foo.status FROM foo")
    assert rewriter.fetchall("SELECT * FROM reports ORDER BY id") == [(2, "approved")]


def test_insert_sink_alias_policy_maps_output_columns():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER, status VARCHAR)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'draft'), (2, 'approved')")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            sink="reports",
            sink_alias="r",
            constraint="r.status = 'approved' AND max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo")
    assert rewriter.fetchall("SELECT * FROM reports ORDER BY id") == [(2, "approved")]


def test_insert_output_marker_policy_maps_output_columns():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER, status VARCHAR)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'draft'), (2, 'approved')")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            sink="reports",
            constraint="_OUTPUT_.status = 'approved' AND max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo")
    assert rewriter.fetchall("SELECT * FROM reports ORDER BY id") == [(2, "approved")]


def test_required_source_missing_fails_closed_on_insert():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE receipts (id INTEGER)")
    rewriter.execute("CREATE TABLE other (id INTEGER)")
    rewriter.execute("CREATE TABLE reports (id INTEGER)")
    rewriter.execute("INSERT INTO other VALUES (1)")
    rewriter.register_policy(
        Policy(
            sources=["receipts"],
            required_sources=["receipts"],
            sink="reports",
            constraint="reports.id > 0 AND max(receipts.id) > 0",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("INSERT INTO reports (id) SELECT other.id FROM other")
    assert rewriter.fetchall("SELECT * FROM reports") == []


def test_required_source_present_enforces_constraint_on_insert():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE receipts (id INTEGER)")
    rewriter.execute("CREATE TABLE reports (id INTEGER)")
    rewriter.execute("INSERT INTO receipts VALUES (5), (20)")
    rewriter.register_policy(
        Policy(
            sources=["receipts"],
            required_sources=["receipts"],
            sink="reports",
            constraint="reports.id > 0 AND max(receipts.id) > 10",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("INSERT INTO reports (id) SELECT receipts.id FROM receipts")
    assert rewriter.fetchall("SELECT * FROM reports") == [(20,)]


def test_update_sink_policy_filters_assignments():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE receipts (id INTEGER)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO reports VALUES (1, 'draft')")
    rewriter.register_policy(
        Policy(
            sources=[],
            sink="reports",
            constraint="reports.status = 'approved'",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("UPDATE reports SET status = 'draft'")
    assert rewriter.fetchall("SELECT * FROM reports") == [(1, "draft")]
    rewriter.execute("UPDATE reports SET status = 'approved'")
    assert rewriter.fetchall("SELECT * FROM reports") == [(1, "approved")]


def test_update_sink_alias_policy_filters_assignments():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO reports VALUES (1, 'draft')")
    rewriter.register_policy(
        Policy(
            sources=[],
            sink="reports",
            sink_alias="r",
            constraint="r.status = 'approved'",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("UPDATE reports SET status = 'draft'")
    assert rewriter.fetchall("SELECT * FROM reports") == [(1, "draft")]
    rewriter.execute("UPDATE reports SET status = 'approved'")
    assert rewriter.fetchall("SELECT * FROM reports") == [(1, "approved")]


def test_update_kill_policy_aborts_invalid_assignment():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO reports VALUES (1, 'draft')")
    rewriter.register_policy(
        Policy(
            sources=[],
            sink="reports",
            constraint="reports.status = 'approved'",
            on_fail=Resolution.KILL,
        )
    )

    with pytest.raises(Exception, match="KILLing due to dfc policy violation"):
        rewriter.execute("UPDATE reports SET status = 'draft'")
    assert rewriter.fetchall("SELECT * FROM reports") == [(1, "draft")]


def test_required_source_missing_fails_closed_on_update():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE receipts (id INTEGER)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO reports VALUES (1, 'old')")
    rewriter.register_policy(
        Policy(
            sources=["receipts"],
            required_sources=["receipts"],
            sink="reports",
            constraint="reports.status = 'approved' AND max(receipts.id) > 0",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("UPDATE reports SET status = 'approved'")
    assert rewriter.fetchall("SELECT * FROM reports") == [(1, "old")]


def test_update_from_source_policy_filters_assignments():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER, status VARCHAR)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'draft'), (2, 'approved')")
    rewriter.execute("INSERT INTO reports VALUES (1, 'old'), (2, 'old')")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            sink="reports",
            constraint="reports.status = 'approved' AND max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("UPDATE reports SET status = foo.status FROM foo WHERE reports.id = foo.id")
    assert rewriter.fetchall("SELECT * FROM reports ORDER BY id") == [
        (1, "old"),
        (2, "approved"),
    ]


def test_remove_policy_filters_before_limit_for_full_push():
    """Distributive policies use Full-Push: inline WHERE is applied before LIMIT."""
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM foo ORDER BY id LIMIT 1") == [(2,)]


def test_remove_policy_filters_before_offset_for_full_push():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2), (3)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 2",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM foo ORDER BY id OFFSET 1") == []


def test_remove_policy_filters_before_limit_offset_for_full_push():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2), (3), (4)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 2",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM foo ORDER BY id LIMIT 2 OFFSET 1") == [(4,)]


def test_full_push_limit_filter_uses_inline_where():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER, secret INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1, 0), (2, 10)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.secret) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM foo ORDER BY id LIMIT 1") == [(2,)]


def test_policy_applies_inside_derived_subquery():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM (SELECT id FROM foo) AS q ORDER BY id") == [(2,)]


def test_policy_applies_inside_cte():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("WITH q AS (SELECT id FROM foo) SELECT id FROM q ORDER BY id") == [
        (2,)
    ]


def test_policy_applies_to_matching_union_branch():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (10)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert sorted(rewriter.fetchall("SELECT id FROM foo UNION ALL SELECT id FROM bar")) == [
        (2,),
        (10,),
    ]


def test_policy_applies_to_matching_intersect_branch():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2), (3)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM foo INTERSECT SELECT id FROM bar ORDER BY id") == [
        (2,)
    ]


def test_policy_applies_to_joined_source_table():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT bar.id FROM bar JOIN foo ON bar.id = foo.id") == [(2,)]


def test_policy_applies_to_each_inner_self_join_alias():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT a.id, b.id FROM foo AS a JOIN foo AS b ON a.id = b.id ORDER BY a.id"
    ) == [(2, 2)]


def test_left_join_policy_preserves_unmatched_left_rows():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2), (3)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT bar.id, foo.id FROM bar LEFT JOIN foo ON bar.id = foo.id ORDER BY bar.id"
    ) == [(1, None), (2, 2), (3, None)]


def test_right_join_policy_preserves_unmatched_right_rows():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2), (3)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT bar.id, foo.id FROM foo RIGHT JOIN bar ON bar.id = foo.id ORDER BY bar.id"
    ) == [(1, None), (2, 2), (3, None)]


def test_rewrites_outer_join_policy_with_source_sets():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["bar", "foo"],
            constraint="max(bar.id) > max(foo.id)",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.transform_query("SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id") == (
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id WHERE bar.id > foo.id"
    )


def test_splits_source_local_outer_join_policy_that_would_need_source_sets():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2), (3)")
    rewriter.register_policy(
        Policy(
            sources=["bar", "foo"],
            constraint="max(bar.id) > 1 AND max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT bar.id, foo.id FROM bar LEFT JOIN foo ON bar.id = foo.id ORDER BY bar.id"
    ) == [(2, 2), (3, None)]


def test_cross_source_outer_join_policy_rewrites_with_source_sets():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["bar", "foo"],
            constraint="max(bar.id) > max(foo.id)",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.transform_query("SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id") == (
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id WHERE bar.id > foo.id"
    )
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2), (3)")
    assert (
        rewriter.fetchall("SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id ORDER BY bar.id")
        == []
    )


def test_splits_source_local_union_policy_that_would_need_source_sets():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (3)")
    rewriter.register_policy(
        Policy(
            sources=["foo", "bar"],
            constraint="max(foo.id) > 1 AND max(bar.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert sorted(rewriter.fetchall("SELECT id FROM foo UNION ALL SELECT id FROM bar")) == [
        (2,),
        (3,),
    ]


def test_splits_source_local_intersect_policy_that_would_need_source_sets():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2), (3)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2), (3)")
    rewriter.register_policy(
        Policy(
            sources=["foo", "bar"],
            constraint="max(foo.id) > 1 AND max(bar.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM foo INTERSECT SELECT id FROM bar ORDER BY id") == [
        (2,),
        (3,),
    ]


def test_cross_source_union_all_passes_through_when_branch_split_unavailable():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["foo", "bar"],
            constraint="max(foo.id) > max(bar.id)",
            on_fail=Resolution.REMOVE,
        )
    )

    assert sorted(rewriter.fetchall("SELECT id FROM foo UNION ALL SELECT id FROM bar")) == []


def test_policy_filters_full_join_source_before_join():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (3)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT bar.id, foo.id FROM bar FULL JOIN foo ON bar.id = foo.id "
        "ORDER BY COALESCE(bar.id, foo.id)"
    ) == [(1, None), (None, 2), (3, None)]


def test_cross_source_full_join_policy_rewrites_with_source_sets():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["foo", "bar"],
            constraint="max(foo.id) > max(bar.id)",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("SELECT bar.id FROM bar FULL JOIN foo ON bar.id = foo.id")
    assert (
        rewriter.fetchall("SELECT bar.id FROM bar FULL JOIN foo ON bar.id = foo.id ORDER BY bar.id")
        == []
    )


def test_policy_applies_to_semi_join_source():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2), (3)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT bar.id FROM bar SEMI JOIN foo ON bar.id = foo.id ORDER BY bar.id"
    ) == [(2,)]


def test_policy_applies_to_right_semi_join_source():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["bar"],
            constraint="max(bar.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert (
        rewriter.transform_query("SELECT foo.id FROM bar RIGHT SEMI JOIN foo ON bar.id = foo.id")
        == "SELECT foo.id FROM bar RIGHT SEMI JOIN foo ON bar.id = foo.id AND bar.id > 1"
    )


def test_transform_query_collect_stats():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    for index in range(50):
        table = f"other_{index}"
        rewriter.connection.execute(f"CREATE TABLE {table} (id INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )
    for index in range(50):
        table = f"other_{index}"
        rewriter.register_policy(
            Policy(
                sources=[table],
                constraint=f"max({table}.id) > 1",
                on_fail=Resolution.REMOVE,
            )
        )

    rewriter.transform_query("SELECT id FROM foo", collect_stats=True)
    stats = rewriter.last_rewrite_stats()
    assert stats is not None
    assert stats.policy_constraints_parsed_during_rewrite == 0
    assert stats.candidate_policies == 1


def test_allows_anti_join_policy_on_preserved_source():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2), (3)")
    rewriter.register_policy(
        Policy(
            sources=["bar"],
            constraint="max(bar.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT bar.id FROM bar ANTI JOIN foo ON bar.id = foo.id ORDER BY bar.id"
    ) == [(3,)]


def test_policy_filters_anti_join_probe_source_before_join():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2), (3)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT bar.id FROM bar ANTI JOIN foo ON bar.id = foo.id ORDER BY bar.id"
    ) == [(1,), (3,)]


def test_policy_applies_inside_exists_subquery():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1)")
    rewriter.execute("INSERT INTO bar VALUES (10)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM bar WHERE EXISTS (SELECT id FROM foo)") == []


def test_policy_applies_inside_not_exists_subquery():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1)")
    rewriter.execute("INSERT INTO bar VALUES (10)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM bar WHERE NOT EXISTS (SELECT id FROM foo)") == [(10,)]


def test_policy_applies_inside_not_in_subquery():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2), (3)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM bar WHERE id NOT IN (SELECT id FROM foo)") == [
        (1,),
        (3,),
    ]


def test_delete_aggregate_policy_updates_rust_storage():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE reports (id INTEGER)")
    policy = AggregatePolicy(
        sources=["foo"],
        sink="reports",
        constraint="sum(reports.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    rewriter.register_policy(policy)
    assert (
        json.loads(rewriter.planner.inner.aggregate_policies_json())[0]["CompatAggregate"][
            "dimensions"
        ]
        == []
    )

    assert rewriter.delete_policy(
        sources=["foo"],
        sink="reports",
        constraint="sum(reports.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    assert rewriter.aggregate_policies() == []


def test_aggregate_policy_temp_columns():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (amount INTEGER)")
    rewriter.execute(
        "CREATE TABLE reports (total INTEGER, __passant_agg_0 INTEGER, __passant_agg_1 INTEGER)"
    )
    rewriter.execute("INSERT INTO foo VALUES (5), (10)")
    rewriter.register_policy(
        AggregatePolicy(
            sources=["foo"],
            sink="reports",
            constraint="sum(foo.amount) >= sum(reports.total)",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("INSERT INTO reports (total) SELECT foo.amount FROM foo")
    assert rewriter.fetchall(
        "SELECT total, __passant_agg_0, __passant_agg_1 FROM reports ORDER BY total"
    ) == [
        (5, 5, 5),
        (10, 10, 10),
    ]


def test_grouped_aggregate_policy_temp_columns():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (region VARCHAR, amount INTEGER)")
    rewriter.execute(
        "CREATE TABLE reports ("
        "region VARCHAR, total INTEGER, __passant_agg_0 INTEGER, __passant_agg_1 INTEGER)"
    )
    rewriter.execute("INSERT INTO foo VALUES ('east', 5), ('east', 10), ('west', 3)")
    rewriter.register_policy(
        AggregatePolicy(
            sources=["foo"],
            sink="reports",
            dimensions=["reports.region"],
            constraint="sum(foo.amount) >= sum(reports.total)",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute(
        "INSERT INTO reports (region, total) "
        "SELECT foo.region, sum(foo.amount) FROM foo GROUP BY foo.region"
    )
    assert rewriter.fetchall(
        "SELECT region, total, __passant_agg_0, __passant_agg_1 FROM reports ORDER BY region"
    ) == [
        ("east", 15, 15, 15),
        ("west", 3, 3, 3),
    ]


def test_count_aggregate_policy_temp_columns():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER, total INTEGER)")
    rewriter.execute(
        "CREATE TABLE reports (total INTEGER, __passant_agg_0 INTEGER, __passant_agg_1 INTEGER)"
    )
    rewriter.execute("INSERT INTO foo VALUES (1, 1), (2, 1), (NULL, 1)")
    rewriter.register_policy(
        AggregatePolicy(
            sources=["foo"],
            sink="reports",
            constraint="count(foo.id) >= sum(reports.total)",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("INSERT INTO reports (total) SELECT foo.total FROM foo")
    assert rewriter.fetchall("SELECT total, __passant_agg_0, __passant_agg_1 FROM reports") == [
        (1, 1, 1),
        (1, 2, 1),
        (1, None, 1),
    ]


def test_multiple_aggregate_policy_temp_columns():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (amount INTEGER, tax INTEGER)")
    rewriter.execute(
        "CREATE TABLE reports (total INTEGER, __passant_agg_0 INTEGER, __passant_agg_1 INTEGER, __passant_agg_2 INTEGER)"
    )
    rewriter.execute("INSERT INTO foo VALUES (5, 1), (10, 2)")
    rewriter.register_policy(
        AggregatePolicy(
            sources=["foo"],
            sink="reports",
            constraint="sum(foo.amount) >= sum(reports.total)",
            on_fail=Resolution.REMOVE,
        )
    )
    rewriter.register_policy(
        AggregatePolicy(
            sources=["foo"],
            sink="reports",
            constraint="sum(foo.tax) >= 3",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("INSERT INTO reports (total) SELECT foo.amount FROM foo")
    assert rewriter.fetchall(
        "SELECT total, __passant_agg_0, __passant_agg_1, __passant_agg_2 FROM reports ORDER BY total"
    ) == [
        (5, 5, 5, 1),
        (10, 10, 10, 2),
    ]


def test_register_policy_rejects_missing_source_table():
    rewriter = wrap(duckdb.connect())
    with pytest.raises(ValueError, match="Source table 'foo' does not exist"):
        rewriter.register_policy(
            Policy(
                sources=["foo"],
                constraint="max(foo.id) > 1",
                on_fail=Resolution.REMOVE,
            )
        )


def test_register_policy_rejects_missing_source_column():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    with pytest.raises(ValueError, match="foo.missing"):
        rewriter.register_policy(
            Policy(
                sources=["foo"],
                constraint="max(foo.missing) > 1",
                on_fail=Resolution.REMOVE,
            )
        )


def test_register_policy_validates_dimension_columns():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    with pytest.raises(ValueError, match="foo.region"):
        rewriter.register_policy(
            Policy(
                sources=["foo"],
                dimensions=["foo.region"],
                constraint="max(foo.id) > 1",
                on_fail=Resolution.REMOVE,
            )
        )


def test_rejects_delete_when_policy_registered():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )
    with pytest.raises(ValueError, match="delete with registered policies"):
        rewriter.execute("DELETE FROM foo WHERE id = 1")


def test_rewrites_except_branch_when_policy_registered():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )
    rewriter.execute("SELECT id FROM bar EXCEPT SELECT id FROM foo")
    assert rewriter.fetchall("SELECT id FROM bar EXCEPT SELECT id FROM foo") == []
