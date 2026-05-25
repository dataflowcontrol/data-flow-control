from passant.compat import AggregateDFCPolicy, DFCPolicy, Resolution, SQLRewriter
import json
import pytest


def test_python_compat_rewriter_preserves_policy_registration():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    policy = DFCPolicy(
        sources=["foo"],
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    rewriter.register_policy(policy)
    assert rewriter.get_dfc_policies() == [policy]
    assert json.loads(rewriter._planner.dfc_policies_json())[0]["CompatDfc"]["sources"] == ["foo"]


def test_python_compat_delete_policy_updates_rust_storage():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1)")
    policy = DFCPolicy(
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
    assert rewriter.get_dfc_policies() == []
    assert rewriter.fetchall("SELECT id FROM foo") == [(1,)]


def test_python_compat_get_pgn_policies_roundtrip():
    from passant.compat import PgnPolicy

    rewriter = SQLRewriter()
    text = (
        "PGN OVER SOURCE foo SINK reports AGGREGATE sum(foo.amount) "
        "CONSTRAINT sum(foo.amount) <= 1000 ON FAIL REMOVE"
    )
    rewriter.register_policy(PgnPolicy.from_text(text))
    assert rewriter.get_pgn_policies() == [PgnPolicy(text=text)]
    assert rewriter._planner.has_registered_policies()
    assert rewriter.get_dfc_policies() == []


def test_python_compat_dfc_policy_from_policy_str_uses_rust_parser():
    policy = DFCPolicy.from_policy_str(
        "SOURCE foo SINK reports CONSTRAINT max(foo.id) > 1 ON FAIL KILL DESCRIPTION stop bad rows"
    )
    assert policy == DFCPolicy(
        sources=["foo"],
        sink="reports",
        constraint="max(foo.id) > 1",
        on_fail=Resolution.KILL,
        description="stop bad rows",
    )


def test_python_compat_dfc_policy_from_policy_str_preserves_required_sources():
    policy = DFCPolicy.from_policy_str(
        "SOURCE REQUIRED receipts SINK reports CONSTRAINT reports.id > 0 ON FAIL REMOVE"
    )
    assert policy == DFCPolicy(
        sources=["receipts"],
        required_sources=["receipts"],
        sink="reports",
        constraint="reports.id > 0",
        on_fail=Resolution.REMOVE,
    )


def test_python_compat_dfc_policy_from_policy_str_preserves_sink_alias():
    policy = DFCPolicy.from_policy_str(
        "SOURCE foo SINK reports AS r CONSTRAINT r.status = 'approved' ON FAIL REMOVE"
    )
    assert policy == DFCPolicy(
        sources=["foo"],
        sink="reports",
        sink_alias="r",
        constraint="r.status = 'approved'",
        on_fail=Resolution.REMOVE,
    )


def test_python_compat_dfc_policy_from_policy_str_normalizes_source_alias():
    policy = DFCPolicy.from_policy_str(
        "SOURCE foo AS f SINK reports CONSTRAINT max(f.id) > 1 ON FAIL REMOVE"
    )
    assert policy == DFCPolicy(
        sources=["foo"],
        sink="reports",
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )


def test_python_compat_dfc_policy_from_policy_str_preserves_dimensions():
    policy = DFCPolicy.from_policy_str(
        "SOURCE foo AS f SINK reports DIMENSION f.region, reports.department "
        "CONSTRAINT max(f.id) > 1 ON FAIL REMOVE"
    )
    assert policy == DFCPolicy(
        sources=["foo"],
        sink="reports",
        dimensions=["foo.region", "reports.department"],
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )


def test_python_compat_aggregate_policy_from_policy_str_uses_rust_parser():
    policy = AggregateDFCPolicy.from_policy_str(
        "AGGREGATE SOURCES foo SINK reports DIMENSION reports.region "
        "CONSTRAINT sum(reports.id) > 1 ON FAIL INVALIDATE"
    )
    assert policy == AggregateDFCPolicy(
        sources=["foo"],
        sink="reports",
        dimensions=["reports.region"],
        constraint="sum(reports.id) > 1",
        on_fail=Resolution.INVALIDATE,
    )


def test_python_compat_policy_requires_sources_list():
    with pytest.raises(ValueError, match="Sources must be provided"):
        DFCPolicy(
            sources=None,
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )


def test_python_compat_policy_validates_constraint_syntax_at_construction():
    with pytest.raises(ValueError, match="Invalid constraint SQL expression"):
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) >",
            on_fail=Resolution.REMOVE,
        )


def test_python_compat_policy_requires_qualified_constraint_columns_at_construction():
    with pytest.raises(ValueError, match="Unqualified columns found: id"):
        DFCPolicy(
            sources=["foo"],
            constraint="max(id) > 1",
            on_fail=Resolution.REMOVE,
        )


def test_python_compat_aggregate_policy_only_supports_invalidate():
    with pytest.raises(ValueError, match="only supports INVALIDATE"):
        AggregateDFCPolicy(
            sources=["foo"],
            constraint="sum(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )


def test_passant_compat_exposes_same_public_api_as_original_sql_rewriter():
    from sql_rewriter import DFCPolicy as LegacyDFCPolicy
    from sql_rewriter import Resolution as LegacyResolution
    from sql_rewriter import SQLRewriter as LegacySQLRewriter

    assert LegacyResolution.REMOVE.value == Resolution.REMOVE.value
    assert LegacyResolution.KILL.value == Resolution.KILL.value
    assert LegacyResolution.INVALIDATE.value == Resolution.INVALIDATE.value
    assert LegacyDFCPolicy.__name__ == DFCPolicy.__name__
    assert LegacySQLRewriter.__name__ == SQLRewriter.__name__


def test_python_compat_stream_path_defaults_and_resets():
    rewriter = SQLRewriter()
    original = rewriter.get_stream_file_path()
    assert isinstance(original, str)
    rewriter.reset_stream_file_path()
    assert isinstance(rewriter.get_stream_file_path(), str)
    assert rewriter.get_stream_file_path() != original


def test_python_compat_transform_query_falls_back_without_extension():
    rewriter = SQLRewriter()
    query = "SELECT id FROM foo"
    assert isinstance(rewriter.transform_query(query), str)


def test_python_compat_transform_query_enforces_registered_policy():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    policy = DFCPolicy(
        sources=["foo"],
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    rewriter.register_policy(policy)
    assert rewriter.transform_query("SELECT id FROM foo") == "SELECT id FROM foo WHERE foo.id > 1"


def test_python_compat_transform_query_collapses_dominated_thresholds():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 10",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.transform_query("SELECT id FROM foo") == "SELECT id FROM foo WHERE foo.id > 10"


def test_python_compat_explain_rewrite_uses_registered_policies():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    explanation = json.loads(rewriter.explain_rewrite("SELECT id FROM foo"))
    assert explanation["chosen"]["rewritten_sql"] == "SELECT id FROM foo WHERE foo.id > 1"
    assert explanation["applicable_policies"][0]["CompatDfc"]["sources"] == ["foo"]


def test_python_compat_explain_rewrite_reports_full_push_strategy_for_join():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    explanation = json.loads(
        rewriter.explain_rewrite("SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id")
    )
    assert explanation["chosen"]["strategy"] == "FullPush"
    assert explanation["scope"]["visible_tables"] == ["foo", "bar"]


def test_python_compat_explain_rewrite_reports_non_distributive_policy_aggregate():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="avg(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    explanation = json.loads(
        rewriter.explain_rewrite("SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id")
    )
    assert explanation["chosen"]["strategy"] == "PartialPush"
    assert explanation["scope"]["policy_aggregate_count"] == 1
    assert explanation["scope"]["policy_aggregates_distributive"] is False
    assert explanation["scope"]["non_distributive_policy_aggregates"] == ["avg(foo.id)"]


def test_python_compat_non_distributive_scan_policy_uses_partial_push():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (3)")
    rewriter.register_policy(
        DFCPolicy(
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


def test_python_compat_non_distributive_partial_push_handles_aliases():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (3)")
    rewriter.register_policy(
        DFCPolicy(
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


def test_python_compat_non_distributive_scalar_fallback_splits_source_local_predicates():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (3)")
    rewriter.execute("INSERT INTO bar VALUES (20), (30)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo", "bar"],
            constraint="avg(foo.id) > 1 AND avg(bar.id) > 10",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT foo.id FROM foo JOIN bar ON foo.id < bar.id ORDER BY foo.id"
    ) == [(1,), (1,), (3,), (3,)]


def test_python_compat_cross_source_non_distributive_aggregate_comparison_uses_partial_push():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (3)")
    rewriter.execute("INSERT INTO bar VALUES (2), (4)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo", "bar"],
            constraint="avg(foo.id) > avg(bar.id)",
            on_fail=Resolution.REMOVE,
        )
    )

    transformed = rewriter.transform_query("SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id")
    assert transformed.startswith("WITH base_query AS (")
    assert "avg(foo.id) > avg(bar.id)" in transformed
    assert rewriter.fetchall("SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id") == []


def test_python_compat_rejects_mixed_row_and_non_distributive_aggregate_policy():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    with pytest.raises(ValueError, match="must be aggregated"):
        rewriter.register_policy(
            DFCPolicy(
                sources=["foo"],
                constraint="foo.id > 0 AND avg(foo.id) > 1",
                on_fail=Resolution.REMOVE,
            )
        )


def test_python_compat_explain_rewrite_reports_unsupported_rewrite_error():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    explanation = json.loads(
        rewriter.explain_rewrite("SELECT id FROM bar EXCEPT SELECT id FROM foo")
    )
    assert explanation["scope"]["requires_source_set_annotations"] is True
    assert explanation["chosen"]["rewrite_error"] is None
    assert explanation["chosen"]["rewritten_sql"] == (
        "SELECT id FROM bar EXCEPT SELECT id FROM foo WHERE foo.id > 1"
    )


def test_python_compat_explain_rewrite_reports_source_set_error():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["bar", "foo"],
            constraint="max(bar.id) > max(foo.id)",
            on_fail=Resolution.REMOVE,
        )
    )

    explanation = json.loads(
        rewriter.explain_rewrite("SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id")
    )
    assert explanation["scope"]["requires_source_set_annotations"] is True
    assert explanation["chosen"]["rewrite_error"] is None
    rewritten = explanation["chosen"]["rewritten_sql"]
    assert "base_query" not in rewritten.lower()
    assert "bar.id > foo.id" in rewritten


def test_python_compat_execute_round_trips_through_duckdb():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    assert rewriter.fetchall("SELECT id FROM foo ORDER BY id") == [(1,), (2,)]


def test_python_compat_kill_resolution_aborts_query():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 10",
            on_fail=Resolution.KILL,
        )
    )

    with pytest.raises(Exception, match="KILLing due to dfc policy violation"):
        rewriter.fetchall("SELECT id FROM foo")


def test_python_compat_llm_resolution_uses_default_resolver_hook():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.LLM,
        )
    )

    assert rewriter.fetchall("SELECT id FROM foo ORDER BY id") == [(2,)]


def test_python_compat_llm_resolution_allows_registered_resolver_hook():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.register_resolver(lambda: True)
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.LLM,
        )
    )

    assert rewriter.fetchall("SELECT id FROM foo ORDER BY id") == [(1,), (2,)]


def test_python_compat_udf_resolution_uses_registered_resolver_hook():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.register_resolver(lambda: True)
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.UDF,
        )
    )

    assert rewriter.fetchall("SELECT id FROM foo ORDER BY id") == [(1,), (2,)]


def test_python_compat_insert_sink_policy_maps_output_columns():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER, status VARCHAR)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'draft'), (2, 'approved')")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            sink="reports",
            constraint="reports.status = 'approved' AND max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo")
    assert rewriter.fetchall("SELECT * FROM reports ORDER BY id") == [(2, "approved")]


def test_python_compat_insert_without_column_list_maps_sink_columns_from_catalog():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER, status VARCHAR)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'draft'), (2, 'approved')")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            sink="reports",
            constraint="reports.status = 'approved' AND max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("INSERT INTO reports SELECT foo.id, foo.status FROM foo")
    assert rewriter.fetchall("SELECT * FROM reports ORDER BY id") == [(2, "approved")]


def test_python_compat_insert_sink_alias_policy_maps_output_columns():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER, status VARCHAR)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'draft'), (2, 'approved')")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            sink="reports",
            sink_alias="r",
            constraint="r.status = 'approved' AND max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo")
    assert rewriter.fetchall("SELECT * FROM reports ORDER BY id") == [(2, "approved")]


def test_python_compat_insert_output_marker_policy_maps_output_columns():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER, status VARCHAR)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'draft'), (2, 'approved')")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            sink="reports",
            constraint="_OUTPUT_.status = 'approved' AND max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo")
    assert rewriter.fetchall("SELECT * FROM reports ORDER BY id") == [(2, "approved")]


def test_python_compat_required_source_missing_fails_closed_on_insert():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE receipts (id INTEGER)")
    rewriter.execute("CREATE TABLE other (id INTEGER)")
    rewriter.execute("CREATE TABLE reports (id INTEGER)")
    rewriter.execute("INSERT INTO other VALUES (1)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["receipts"],
            required_sources=["receipts"],
            sink="reports",
            constraint="reports.id > 0 AND max(receipts.id) > 0",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("INSERT INTO reports (id) SELECT other.id FROM other")
    assert rewriter.fetchall("SELECT * FROM reports") == []


def test_python_compat_required_source_present_enforces_constraint_on_insert():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE receipts (id INTEGER)")
    rewriter.execute("CREATE TABLE reports (id INTEGER)")
    rewriter.execute("INSERT INTO receipts VALUES (5), (20)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["receipts"],
            required_sources=["receipts"],
            sink="reports",
            constraint="reports.id > 0 AND max(receipts.id) > 10",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("INSERT INTO reports (id) SELECT receipts.id FROM receipts")
    assert rewriter.fetchall("SELECT * FROM reports") == [(20,)]


def test_python_compat_insert_invalidate_adds_valid_output():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER, status VARCHAR)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR, valid BOOLEAN)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'draft'), (2, 'approved')")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            sink="reports",
            constraint="reports.status = 'approved' AND max(foo.id) > 1",
            on_fail=Resolution.INVALIDATE,
        )
    )

    rewriter.execute("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo")
    assert rewriter.fetchall("SELECT * FROM reports ORDER BY id") == [
        (1, "draft", False),
        (2, "approved", True),
    ]


def test_python_compat_insert_invalidate_message_adds_message_output():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER, status VARCHAR)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR, invalid_string VARCHAR)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'draft'), (2, 'approved')")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            sink="reports",
            constraint="reports.status = 'approved' AND max(foo.id) > 1",
            on_fail=Resolution.INVALIDATE_MESSAGE,
            description="bad row",
        )
    )

    rewriter.execute("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo")
    assert rewriter.fetchall("SELECT * FROM reports ORDER BY id") == [
        (1, "draft", "bad row"),
        (2, "approved", None),
    ]


def test_python_compat_update_sink_policy_filters_assignments():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE receipts (id INTEGER)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO reports VALUES (1, 'draft')")
    rewriter.register_policy(
        DFCPolicy(
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


def test_python_compat_update_sink_alias_policy_filters_assignments():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO reports VALUES (1, 'draft')")
    rewriter.register_policy(
        DFCPolicy(
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


def test_python_compat_update_kill_policy_aborts_invalid_assignment():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO reports VALUES (1, 'draft')")
    rewriter.register_policy(
        DFCPolicy(
            sources=[],
            sink="reports",
            constraint="reports.status = 'approved'",
            on_fail=Resolution.KILL,
        )
    )

    with pytest.raises(Exception, match="KILLing due to dfc policy violation"):
        rewriter.execute("UPDATE reports SET status = 'draft'")
    assert rewriter.fetchall("SELECT * FROM reports") == [(1, "draft")]


def test_python_compat_update_udf_policy_allows_registered_resolver():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO reports VALUES (1, 'draft')")
    rewriter.register_resolver(lambda: True)
    rewriter.register_policy(
        DFCPolicy(
            sources=[],
            sink="reports",
            constraint="reports.status = 'approved'",
            on_fail=Resolution.UDF,
        )
    )

    rewriter.execute("UPDATE reports SET status = 'draft'")
    assert rewriter.fetchall("SELECT * FROM reports") == [(1, "draft")]


def test_python_compat_required_source_missing_fails_closed_on_update():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE receipts (id INTEGER)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO reports VALUES (1, 'old')")
    rewriter.register_policy(
        DFCPolicy(
            sources=["receipts"],
            required_sources=["receipts"],
            sink="reports",
            constraint="reports.status = 'approved' AND max(receipts.id) > 0",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("UPDATE reports SET status = 'approved'")
    assert rewriter.fetchall("SELECT * FROM reports") == [(1, "old")]


def test_python_compat_update_from_source_policy_filters_assignments():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER, status VARCHAR)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'draft'), (2, 'approved')")
    rewriter.execute("INSERT INTO reports VALUES (1, 'old'), (2, 'old')")
    rewriter.register_policy(
        DFCPolicy(
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


def test_python_compat_update_invalidate_maintains_existing_valid_assignment():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR, valid BOOLEAN)")
    rewriter.execute("INSERT INTO reports VALUES (1, 'draft', TRUE)")
    rewriter.register_policy(
        DFCPolicy(
            sources=[],
            sink="reports",
            constraint="reports.status = 'approved'",
            on_fail=Resolution.INVALIDATE,
        )
    )

    rewriter.execute("UPDATE reports SET valid = FALSE, status = 'approved'")
    assert rewriter.fetchall("SELECT id, status, valid FROM reports") == [(1, "approved", False)]


def test_python_compat_update_from_source_invalidate_policy_sets_valid():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER, status VARCHAR)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR, valid BOOLEAN)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'draft'), (2, 'approved')")
    rewriter.execute("INSERT INTO reports VALUES (1, 'old', TRUE), (2, 'old', TRUE)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            sink="reports",
            constraint="reports.status = 'approved' AND max(foo.id) > 1",
            on_fail=Resolution.INVALIDATE,
        )
    )

    rewriter.execute("UPDATE reports SET status = foo.status FROM foo WHERE reports.id = foo.id")
    assert rewriter.fetchall("SELECT * FROM reports ORDER BY id") == [
        (1, "draft", False),
        (2, "approved", True),
    ]


def test_python_compat_update_from_source_invalidate_message_policy_sets_message():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER, status VARCHAR)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR, invalid_string VARCHAR)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'draft'), (2, 'approved')")
    rewriter.execute("INSERT INTO reports VALUES (1, 'old', NULL), (2, 'old', NULL)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            sink="reports",
            constraint="reports.status = 'approved' AND max(foo.id) > 1",
            on_fail=Resolution.INVALIDATE_MESSAGE,
            description="bad update",
        )
    )

    rewriter.execute("UPDATE reports SET status = foo.status FROM foo WHERE reports.id = foo.id")
    assert rewriter.fetchall("SELECT * FROM reports ORDER BY id") == [
        (1, "draft", "bad update"),
        (2, "approved", None),
    ]


def test_python_compat_update_invalidate_message_maintains_existing_assignment():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR, invalid_string VARCHAR)")
    rewriter.execute("INSERT INTO reports VALUES (1, 'approved', NULL)")
    rewriter.register_policy(
        DFCPolicy(
            sources=[],
            sink="reports",
            constraint="reports.status = 'approved'",
            on_fail=Resolution.INVALIDATE_MESSAGE,
            description="bad status",
        )
    )

    rewriter.execute("UPDATE reports SET invalid_string = 'prior', status = 'draft'")
    assert rewriter.fetchall("SELECT id, status, invalid_string FROM reports") == [
        (1, "draft", "prior; bad status")
    ]


def test_python_compat_invalidate_message_adds_message_column():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.INVALIDATE_MESSAGE,
            description="id too small",
        )
    )

    assert rewriter.fetchall("SELECT id FROM foo ORDER BY id") == [
        (1, "id too small"),
        (2, None),
    ]


def test_python_compat_invalidate_maintains_existing_valid_column():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER, valid BOOLEAN)")
    rewriter.execute("INSERT INTO foo VALUES (1, TRUE), (2, FALSE)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.INVALIDATE,
        )
    )

    assert rewriter.fetchall("SELECT id, valid FROM foo ORDER BY id") == [
        (1, False),
        (2, False),
    ]


def test_python_compat_invalidate_message_maintains_existing_message_column():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER, invalid_string VARCHAR)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'prior'), (2, NULL)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.INVALIDATE_MESSAGE,
            description="id too small",
        )
    )

    assert rewriter.fetchall("SELECT id, invalid_string FROM foo ORDER BY id") == [
        (1, "prior; id too small"),
        (2, None),
    ]


def test_python_compat_remove_policy_filters_before_limit_for_full_push():
    """Distributive policies use Full-Push: inline WHERE is applied before LIMIT."""
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM foo ORDER BY id LIMIT 1") == [(2,)]


def test_python_compat_remove_policy_filters_before_offset_for_full_push():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2), (3)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 2",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM foo ORDER BY id OFFSET 1") == []


def test_python_compat_remove_policy_filters_before_limit_offset_for_full_push():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2), (3), (4)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 2",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM foo ORDER BY id LIMIT 2 OFFSET 1") == [(4,)]


def test_python_compat_full_push_limit_filter_uses_inline_where():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER, secret INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1, 0), (2, 10)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.secret) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM foo ORDER BY id LIMIT 1") == [(2,)]


def test_python_compat_policy_applies_inside_derived_subquery():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM (SELECT id FROM foo) AS q ORDER BY id") == [(2,)]


def test_python_compat_policy_applies_inside_cte():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("WITH q AS (SELECT id FROM foo) SELECT id FROM q ORDER BY id") == [
        (2,)
    ]


def test_python_compat_policy_applies_to_matching_union_branch():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (10)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert sorted(rewriter.fetchall("SELECT id FROM foo UNION ALL SELECT id FROM bar")) == [
        (2,),
        (10,),
    ]


def test_python_compat_policy_applies_to_matching_intersect_branch():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2), (3)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM foo INTERSECT SELECT id FROM bar ORDER BY id") == [
        (2,)
    ]


def test_python_compat_policy_applies_to_joined_source_table():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT bar.id FROM bar JOIN foo ON bar.id = foo.id") == [(2,)]


def test_python_compat_policy_applies_to_each_inner_self_join_alias():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT a.id, b.id FROM foo AS a JOIN foo AS b ON a.id = b.id ORDER BY a.id"
    ) == [(2, 2)]


def test_python_compat_left_join_policy_preserves_unmatched_left_rows():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2), (3)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT bar.id, foo.id FROM bar LEFT JOIN foo ON bar.id = foo.id ORDER BY bar.id"
    ) == [(1, None), (2, 2), (3, None)]


def test_python_compat_right_join_policy_preserves_unmatched_right_rows():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2), (3)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT bar.id, foo.id FROM foo RIGHT JOIN bar ON bar.id = foo.id ORDER BY bar.id"
    ) == [(1, None), (2, 2), (3, None)]


def test_python_compat_rewrites_outer_join_policy_with_source_sets():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["bar", "foo"],
            constraint="max(bar.id) > max(foo.id)",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.transform_query("SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id") == (
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id WHERE bar.id > foo.id"
    )


def test_python_compat_splits_source_local_outer_join_policy_that_would_need_source_sets():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2), (3)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["bar", "foo"],
            constraint="max(bar.id) > 1 AND max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT bar.id, foo.id FROM bar LEFT JOIN foo ON bar.id = foo.id ORDER BY bar.id"
    ) == [(2, 2), (3, None)]


def test_python_compat_cross_source_outer_join_policy_rewrites_with_source_sets():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        DFCPolicy(
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


def test_python_compat_splits_source_local_union_policy_that_would_need_source_sets():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (3)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo", "bar"],
            constraint="max(foo.id) > 1 AND max(bar.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert sorted(rewriter.fetchall("SELECT id FROM foo UNION ALL SELECT id FROM bar")) == [
        (2,),
        (3,),
    ]


def test_python_compat_splits_source_local_intersect_policy_that_would_need_source_sets():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2), (3)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2), (3)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo", "bar"],
            constraint="max(foo.id) > 1 AND max(bar.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM foo INTERSECT SELECT id FROM bar ORDER BY id") == [
        (2,),
        (3,),
    ]


def test_python_compat_cross_source_union_all_passes_through_when_branch_split_unavailable():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo", "bar"],
            constraint="max(foo.id) > max(bar.id)",
            on_fail=Resolution.REMOVE,
        )
    )

    assert sorted(rewriter.fetchall("SELECT id FROM foo UNION ALL SELECT id FROM bar")) == []


def test_python_compat_policy_filters_full_join_source_before_join():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (3)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT bar.id, foo.id FROM bar FULL JOIN foo ON bar.id = foo.id "
        "ORDER BY COALESCE(bar.id, foo.id)"
    ) == [(1, None), (None, 2), (3, None)]


def test_python_compat_cross_source_full_join_policy_rewrites_with_source_sets():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        DFCPolicy(
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


def test_python_compat_policy_applies_to_semi_join_source():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2), (3)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT bar.id FROM bar SEMI JOIN foo ON bar.id = foo.id ORDER BY bar.id"
    ) == [(2,)]


def test_python_compat_policy_applies_to_right_semi_join_source():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["bar"],
            constraint="max(bar.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert (
        rewriter.transform_query("SELECT foo.id FROM bar RIGHT SEMI JOIN foo ON bar.id = foo.id")
        == "SELECT foo.id FROM bar RIGHT SEMI JOIN foo ON bar.id = foo.id AND bar.id > 1"
    )


def test_python_compat_transform_query_collect_stats():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    for index in range(50):
        table = f"other_{index}"
        rewriter.conn.execute(f"CREATE TABLE {table} (id INTEGER)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )
    for index in range(50):
        table = f"other_{index}"
        rewriter.register_policy(
            DFCPolicy(
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


def test_python_compat_allows_anti_join_policy_on_preserved_source():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2), (3)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["bar"],
            constraint="max(bar.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT bar.id FROM bar ANTI JOIN foo ON bar.id = foo.id ORDER BY bar.id"
    ) == [(3,)]


def test_python_compat_policy_filters_anti_join_probe_source_before_join():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2), (3)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall(
        "SELECT bar.id FROM bar ANTI JOIN foo ON bar.id = foo.id ORDER BY bar.id"
    ) == [(1,), (3,)]


def test_python_compat_policy_applies_inside_exists_subquery():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1)")
    rewriter.execute("INSERT INTO bar VALUES (10)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM bar WHERE EXISTS (SELECT id FROM foo)") == []


def test_python_compat_policy_applies_inside_not_exists_subquery():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1)")
    rewriter.execute("INSERT INTO bar VALUES (10)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM bar WHERE NOT EXISTS (SELECT id FROM foo)") == [(10,)]


def test_python_compat_policy_applies_inside_not_in_subquery():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (2)")
    rewriter.execute("INSERT INTO bar VALUES (1), (2), (3)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id FROM bar WHERE id NOT IN (SELECT id FROM foo)") == [
        (1,),
        (3,),
    ]


def test_python_compat_finalize_aggregate_policies_returns_mapping():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE reports (id INTEGER)")
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


def test_python_compat_delete_aggregate_policy_updates_rust_storage():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE reports (id INTEGER)")
    policy = AggregateDFCPolicy(
        sources=["foo"],
        sink="reports",
        constraint="sum(reports.id) > 1",
        on_fail=Resolution.INVALIDATE,
    )
    rewriter.register_policy(policy)
    assert (
        json.loads(rewriter._planner.aggregate_policies_json())[0]["CompatAggregate"]["dimensions"]
        == []
    )

    assert rewriter.delete_policy(
        sources=["foo"],
        sink="reports",
        constraint="sum(reports.id) > 1",
        on_fail=Resolution.INVALIDATE,
    )
    assert rewriter.get_aggregate_policies() == []
    assert rewriter.finalize_aggregate_policies("reports") == {}


def test_python_compat_finalize_aggregate_policies_reports_violation():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE reports (id INTEGER)")
    rewriter.execute("INSERT INTO reports VALUES (1)")
    rewriter.register_policy(
        AggregateDFCPolicy(
            sources=[],
            sink="reports",
            constraint="sum(reports.id) > 10",
            on_fail=Resolution.INVALIDATE,
            description="too small",
        )
    )
    assert rewriter.finalize_aggregate_policies("reports") == {
        "aggregate::sum(reports.id) > 10": (
            "too small: Aggregate policy constraint violated: sum(reports.id) > 10"
        )
    }


def test_python_compat_finalize_aggregate_policies_invalidates_sink_rows():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE reports (id INTEGER, valid BOOLEAN)")
    rewriter.execute("INSERT INTO reports VALUES (1, TRUE), (2, TRUE)")
    rewriter.register_policy(
        AggregateDFCPolicy(
            sources=[],
            sink="reports",
            constraint="sum(reports.id) > 10",
            on_fail=Resolution.INVALIDATE,
        )
    )

    assert rewriter.finalize_aggregate_policies("reports") == {
        "aggregate::sum(reports.id) > 10": (
            "Aggregate policy constraint violated: sum(reports.id) > 10"
        )
    }
    assert rewriter.fetchall("SELECT valid FROM reports ORDER BY id") == [(False,), (False,)]


def test_python_compat_finalize_aggregate_policies_preserves_prior_invalidations():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE reports (id INTEGER, valid BOOLEAN)")
    rewriter.execute("INSERT INTO reports VALUES (1, TRUE), (2, TRUE)")
    rewriter.register_policy(
        AggregateDFCPolicy(
            sources=[],
            sink="reports",
            constraint="sum(reports.id) > 10",
            on_fail=Resolution.INVALIDATE,
        )
    )
    rewriter.register_policy(
        AggregateDFCPolicy(
            sources=[],
            sink="reports",
            constraint="sum(reports.id) > 1",
            on_fail=Resolution.INVALIDATE,
        )
    )

    rewriter.finalize_aggregate_policies("reports")
    assert rewriter.fetchall("SELECT valid FROM reports ORDER BY id") == [(False,), (False,)]


def test_python_compat_finalize_dimensioned_aggregate_policies_invalidates_matching_groups():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE reports (region VARCHAR, total INTEGER, valid BOOLEAN)")
    rewriter.execute(
        "INSERT INTO reports VALUES "
        "('east', 40, TRUE), ('east', 70, TRUE), "
        "('west', 20, TRUE), ('west', 30, TRUE)"
    )
    rewriter.register_policy(
        AggregateDFCPolicy(
            sources=[],
            sink="reports",
            dimensions=["reports.region"],
            constraint="sum(reports.total) > 100",
            on_fail=Resolution.INVALIDATE,
        )
    )

    assert rewriter.finalize_aggregate_policies("reports") == {
        "aggregate::sum(reports.total) > 100": (
            "Aggregate policy constraint violated: sum(reports.total) > 100"
        )
    }
    assert rewriter.fetchall("SELECT region, valid FROM reports ORDER BY region, total") == [
        ("east", True),
        ("east", True),
        ("west", False),
        ("west", False),
    ]


def test_python_compat_aggregate_policy_temp_columns_and_finalize():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (amount INTEGER)")
    rewriter.execute(
        "CREATE TABLE reports (total INTEGER, __passant_agg_0 INTEGER, __passant_agg_1 INTEGER)"
    )
    rewriter.execute("INSERT INTO foo VALUES (5), (10)")
    rewriter.register_policy(
        AggregateDFCPolicy(
            sources=["foo"],
            sink="reports",
            constraint="sum(foo.amount) >= sum(reports.total)",
            on_fail=Resolution.INVALIDATE,
        )
    )

    rewriter.execute("INSERT INTO reports (total) SELECT foo.amount FROM foo")
    assert rewriter.fetchall(
        "SELECT total, __passant_agg_0, __passant_agg_1 FROM reports ORDER BY total"
    ) == [
        (5, 5, 5),
        (10, 10, 10),
    ]
    assert rewriter.finalize_aggregate_policies("reports") == {
        "aggregate::sum(foo.amount) >= sum(reports.total)": None
    }


def test_python_compat_grouped_aggregate_policy_temp_columns_and_finalize():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (region VARCHAR, amount INTEGER)")
    rewriter.execute(
        "CREATE TABLE reports ("
        "region VARCHAR, total INTEGER, __passant_agg_0 INTEGER, __passant_agg_1 INTEGER, valid BOOLEAN)"
    )
    rewriter.execute("INSERT INTO foo VALUES ('east', 5), ('east', 10), ('west', 3)")
    rewriter.register_policy(
        AggregateDFCPolicy(
            sources=["foo"],
            sink="reports",
            dimensions=["reports.region"],
            constraint="sum(foo.amount) >= sum(reports.total)",
            on_fail=Resolution.INVALIDATE,
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
    assert rewriter.finalize_aggregate_policies("reports") == {
        "aggregate::sum(foo.amount) >= sum(reports.total)": None
    }
    assert rewriter.fetchall("SELECT region, valid FROM reports ORDER BY region") == [
        ("east", True),
        ("west", True),
    ]


def test_python_compat_count_aggregate_policy_temp_columns_and_finalize():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER, total INTEGER)")
    rewriter.execute(
        "CREATE TABLE reports (total INTEGER, __passant_agg_0 INTEGER, __passant_agg_1 INTEGER)"
    )
    rewriter.execute("INSERT INTO foo VALUES (1, 1), (2, 1), (NULL, 1)")
    rewriter.register_policy(
        AggregateDFCPolicy(
            sources=["foo"],
            sink="reports",
            constraint="count(foo.id) >= sum(reports.total)",
            on_fail=Resolution.INVALIDATE,
        )
    )

    rewriter.execute("INSERT INTO reports (total) SELECT foo.total FROM foo")
    assert rewriter.fetchall("SELECT total, __passant_agg_0, __passant_agg_1 FROM reports") == [
        (1, 1, 1),
        (1, 2, 1),
        (1, None, 1),
    ]
    assert rewriter.finalize_aggregate_policies("reports") == {
        "aggregate::count(foo.id) >= sum(reports.total)": None
    }


def test_python_compat_multiple_aggregate_policy_temp_columns_and_finalize():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (amount INTEGER, tax INTEGER)")
    rewriter.execute(
        "CREATE TABLE reports (total INTEGER, __passant_agg_0 INTEGER, __passant_agg_1 INTEGER, __passant_agg_2 INTEGER)"
    )
    rewriter.execute("INSERT INTO foo VALUES (5, 1), (10, 2)")
    rewriter.register_policy(
        AggregateDFCPolicy(
            sources=["foo"],
            sink="reports",
            constraint="sum(foo.amount) >= sum(reports.total)",
            on_fail=Resolution.INVALIDATE,
        )
    )
    rewriter.register_policy(
        AggregateDFCPolicy(
            sources=["foo"],
            sink="reports",
            constraint="sum(foo.tax) >= 3",
            on_fail=Resolution.INVALIDATE,
        )
    )

    rewriter.execute("INSERT INTO reports (total) SELECT foo.amount FROM foo")
    assert rewriter.fetchall(
        "SELECT total, __passant_agg_0, __passant_agg_1, __passant_agg_2 FROM reports ORDER BY total"
    ) == [
        (5, 5, 5, 1),
        (10, 10, 10, 2),
    ]
    assert rewriter.finalize_aggregate_policies("reports") == {
        "aggregate::sum(foo.amount) >= sum(reports.total)": None,
        "aggregate::sum(foo.tax) >= 3": None,
    }


def test_python_compat_register_policy_rejects_missing_source_table():
    rewriter = SQLRewriter()
    with pytest.raises(ValueError, match="Source table 'foo' does not exist"):
        rewriter.register_policy(
            DFCPolicy(
                sources=["foo"],
                constraint="max(foo.id) > 1",
                on_fail=Resolution.REMOVE,
            )
        )


def test_python_compat_register_policy_rejects_missing_source_column():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    with pytest.raises(ValueError, match="foo.missing"):
        rewriter.register_policy(
            DFCPolicy(
                sources=["foo"],
                constraint="max(foo.missing) > 1",
                on_fail=Resolution.REMOVE,
            )
        )


def test_python_compat_register_policy_validates_dimension_columns():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    with pytest.raises(ValueError, match="foo.region"):
        rewriter.register_policy(
            DFCPolicy(
                sources=["foo"],
                dimensions=["foo.region"],
                constraint="max(foo.id) > 1",
                on_fail=Resolution.REMOVE,
            )
        )


def test_python_compat_rejects_delete_when_policy_registered():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )
    with pytest.raises(ValueError, match="delete with registered policies"):
        rewriter.execute("DELETE FROM foo WHERE id = 1")


def test_python_compat_rewrites_except_branch_when_policy_registered():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("CREATE TABLE bar (id INTEGER)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )
    rewriter.execute("SELECT id FROM bar EXCEPT SELECT id FROM foo")
    assert rewriter.fetchall("SELECT id FROM bar EXCEPT SELECT id FROM foo") == []
