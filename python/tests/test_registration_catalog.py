"""Policy registration requires explicit SOURCE/SINK schemas and is atomic."""

import duckdb
import pytest
from data_flow_control import PassantRewriteError, Policy, Resolution, dfc
from data_flow_control.planner import Planner


@pytest.mark.parametrize(
    "resolution", ["REMOVE", "KILL", "RETRY('exact')", "UDF repair", "RELATION UDF repair", "UI"]
)
def test_bare_planner_cannot_register_physical_policy_without_catalog(resolution):
    planner = Planner()
    policy = Policy.from_pgn(f"SOURCE missing SINK target CONSTRAINT TRUE ON FAIL {resolution}")
    with pytest.raises(PassantRewriteError, match="Source table 'missing' does not exist"):
        planner.register_policy(policy)
    assert planner.policies() == []
    assert planner.policy_by_index(0) is None


@pytest.mark.parametrize(
    "invalid",
    [
        "SOURCE absent CONSTRAINT TRUE ON FAIL REMOVE",
        "SINK absent CONSTRAINT TRUE ON FAIL REMOVE",
        "SOURCE a CONSTRAINT a.missing > 0 ON FAIL REMOVE",
        "SINK target CONSTRAINT target.missing > 0 ON FAIL REMOVE",
    ],
)
def test_actual_catalog_rejects_unknown_table_or_column_without_consuming_id(invalid):
    raw = duckdb.connect()
    raw.execute("CREATE TABLE a(x INT); CREATE TABLE target(x INT)")
    with dfc(raw) as conn:
        with pytest.raises(PassantRewriteError):
            conn.register_policy(Policy.from_pgn(invalid))
        assert conn.policies() == []
        conn.register_policy(
            Policy(
                sources=["a"],
                sink="target",
                constraint="a.x > 0",
                on_fail=Resolution.RETRY,
                retry_message="exact",
            )
        )
        assert conn.planner.policy_by_index(0).retry_message == "exact"
        assert conn.planner.policy_by_index(1) is None


def test_failed_registration_batch_preserves_existing_store_and_stable_ids():
    raw = duckdb.connect()
    raw.execute("CREATE TABLE a(x INT); CREATE TABLE target(x INT)")
    with dfc(raw) as conn:
        good = Policy(
            sources=["a"],
            sink="target",
            constraint="a.x > 0",
            on_fail=Resolution.RETRY,
            retry_message="same",
        )
        conn.register_policy(good)
        bad = Policy(
            sources=["a"], sink="target", constraint="a.missing > 0", on_fail=Resolution.REMOVE
        )
        with pytest.raises(PassantRewriteError):
            conn.register_policies([good, bad])
        assert len(conn.policies()) == 1
        assert conn.planner.policy_by_index(1) is None
        conn.register_policy(good)
        assert conn.planner.policy_by_index(1).retry_message == "same"


@pytest.mark.parametrize("source", ["s.a", '"s"."a"'])
def test_actual_schema_alias_and_quoted_column_registration(source):
    raw = duckdb.connect()
    raw.execute(
        'CREATE SCHEMA s; CREATE TABLE s.a("Quoted Column" INT); CREATE TABLE target(x INT)'
    )
    with dfc(raw) as conn:
        conn.register_policy(
            Policy.from_pgn(
                f'SOURCE {source} AS a SINK target AS t CONSTRAINT a."Quoted Column" > 0 AND t.x > 0 ON FAIL REMOVE'
            )
        )
        with pytest.raises(PassantRewriteError, match="does not exist in source"):
            conn.register_policy(
                Policy.from_pgn(
                    f'SOURCE {source} AS a SINK target AS t CONSTRAINT a."Wrong Column" > 0 ON FAIL REMOVE'
                )
            )
        assert len(conn.policies()) == 1
