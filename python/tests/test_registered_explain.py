"""Registered explanations must compile using the same catalog/function context."""

import duckdb
import pytest
from data_flow_control import PassantRewriteError, Policy, Resolution, RewriteOptions, dfc


def _chosen_options(explanation):
    return RewriteOptions(use_partial_push=explanation["chosen"]["strategy"] == "PartialPush")


def test_registered_explain_uses_catalog_outer_markers_without_execution(monkeypatch):
    raw = duckdb.connect()
    raw.execute("CREATE TABLE a(v INT); CREATE TABLE b(v INT)")
    with dfc(raw) as conn:
        conn.register_policy(
            Policy(sources=["a", "b"], constraint="FALSE", on_fail=Resolution.REMOVE)
        )
        query = "SELECT a.v AS av,b.v AS bv FROM a FULL JOIN b ON TRUE"

        def no_execute(*args, **kwargs):
            raise AssertionError("explain must not submit SQL")

        monkeypatch.setattr(conn.adapter, "execute", no_execute)
        explanation = conn.explain(query)
        assert explanation["chosen"]["rewrite_error"] is None
        assert explanation["chosen"]["rewritten_sql"] == conn.transform_query(
            query, options=_chosen_options(explanation)
        )
        assert "__passant_lineage_" in explanation["chosen"]["rewritten_sql"]
        assert not any(step["stage"] == "fallback" for step in explanation["steps"])
        assert explanation["statement_plan"]


def test_registered_explain_retains_function_metadata_and_does_not_invoke_udf():
    raw = duckdb.connect()
    raw.execute("CREATE TABLE a(v BIGINT); CREATE TABLE b(v BIGINT)")
    calls = []
    with dfc(raw) as conn:
        conn.register_resolution_function(
            "defined_first", lambda value: value, ["BIGINT"], "BIGINT", splittable=False
        )
        conn.register_resolution_function(
            "repair",
            lambda value: calls.append(value) or value,
            ["BIGINT"],
            "BIGINT",
            splittable=True,
        )
        conn.register_policy(
            Policy(
                sources=["a", "b"], constraint="FALSE", on_fail=Resolution.UDF, udf_name="repair"
            )
        )
        query = "SELECT a.v AS av FROM a LEFT JOIN b ON TRUE"
        explanation = conn.explain(query)
        assert explanation["chosen"]["rewrite_error"] is None
        assert explanation["chosen"]["rewritten_sql"] == conn.transform_query(
            query, options=_chosen_options(explanation)
        )
        native = conn.planner.inner
        assert native.resolution_registration_order("defined_first") == 0
        assert native.resolution_registration_order("repair") == 1
        assert native.resolution_splittable("UDF repair")
        assert calls == []


def test_registered_explain_reports_same_unsupported_shape_as_transform():
    raw = duckdb.connect()
    raw.execute("CREATE TABLE a(v INT); CREATE TABLE b(v INT)")
    with dfc(raw) as conn:
        conn.register_policy(
            Policy(
                sources=["a", "b"],
                constraint="FALSE",
                on_fail=Resolution.RETRY,
                retry_message="blocked",
            )
        )
        query = "SELECT MAX(a.v) AS av FROM a LEFT JOIN b ON TRUE"
        explanation = conn.explain(query)
        assert explanation["chosen"]["rewrite_error"]
        with pytest.raises(PassantRewriteError) as caught:
            conn.transform_query(query, options=_chosen_options(explanation))
        assert explanation["chosen"]["rewrite_error"] == str(caught.value)
        assert any(
            step["stage"] == "fallback" and str(caught.value) in step["detail"]
            for step in explanation["steps"]
        )
