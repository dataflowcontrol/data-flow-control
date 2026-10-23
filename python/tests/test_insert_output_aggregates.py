"""Execution regressions for policies deferred past an INSERT aggregation CTE."""

import duckdb
import pytest

from data_flow_control import Policy, Resolution, dfc


def connection():
    db = dfc(duckdb.connect())
    db.execute("CREATE TABLE source (x INTEGER)")
    db.execute("INSERT INTO source VALUES (1), (2)")
    db.execute("CREATE TABLE sink (y INTEGER)")
    return db


def insert(projection="x", aggregate="COUNT(*) AS n"):
    return (
        f"INSERT INTO sink WITH candidate AS (SELECT {projection} FROM source), "
        f"rollup AS (SELECT {aggregate} FROM candidate) SELECT 1 AS y FROM rollup"
    )


def policy(constraint):
    return Policy(sources=["source"], sink="sink", constraint=constraint, on_fail=Resolution.KILL)


@pytest.mark.parametrize("separate_policies", [False, True])
def test_different_aggregate_expressions_cannot_share_an_alias(separate_policies):
    db = connection()
    constraints = ["MIN(source.x + 1) > 0", "MIN(source.x - 100) > 0"]
    if separate_policies:
        for constraint in constraints:
            db.register_policy(policy(constraint + " AND sink.y > 0"))
    else:
        db.register_policy(policy(" AND ".join(constraints) + " AND sink.y > 0"))
    with pytest.raises(Exception, match="KILLing due to dfc policy violation"):
        db.execute(insert())
    assert db.raw_connection.execute("SELECT * FROM sink").fetchall() == []


@pytest.mark.parametrize(
    "constraint,passes",
    [
        ("COUNT(source.x) = 2", True),
        ("COUNT(source.x) > 2", False),
        ("COUNT(DISTINCT source.x) = 2", True),
        ("COALESCE(MIN(source.x), 0) > 0", True),
        ("COALESCE(MIN(source.x), 0) > 10", False),
        ("MIN(source.x) BETWEEN 1 AND 3", True),
        ("MIN(source.x) IN (10, 20)", False),
    ],
)
def test_deferred_aggregate_forms(constraint, passes):
    db = connection()
    db.register_policy(policy(constraint + " AND sink.y > 0"))
    if passes:
        db.execute(insert())
        assert db.raw_connection.execute("SELECT * FROM sink").fetchall() == [(1,)]
    else:
        with pytest.raises(Exception, match="KILLing due to dfc policy violation"):
            db.execute(insert())
        assert db.raw_connection.execute("SELECT * FROM sink").fetchall() == []


@pytest.mark.parametrize("projection", ["x AS renamed", "source.x AS renamed"])
@pytest.mark.parametrize("threshold,passes", [(0, True), (10, False)])
def test_source_columns_follow_projection_aliases(projection, threshold, passes):
    db = connection()
    db.register_policy(policy(f"MIN(source.x) > {threshold} AND sink.y > 0"))
    if passes:
        db.execute(insert(projection))
        assert db.raw_connection.execute("SELECT * FROM sink").fetchall() == [(1,)]
    else:
        with pytest.raises(Exception, match="KILLing due to dfc policy violation"):
            db.execute(insert(projection))
        assert db.raw_connection.execute("SELECT * FROM sink").fetchall() == []


def test_existing_projection_cannot_override_generated_alias():
    db = connection()
    db.register_policy(policy("MIN(source.x) > 10 AND sink.y > 0"))
    with pytest.raises(Exception, match="KILLing due to dfc policy violation"):
        db.execute(insert(aggregate="COUNT(*) AS n, 999 AS __passant_min_x"))
    assert db.raw_connection.execute("SELECT * FROM sink").fetchall() == []


def test_source_aliases_follow_multiple_cte_renames():
    db = connection()
    db.register_policy(policy('MIN(source.x) > 0 AND "sink"."y" > 0'))
    db.execute(
        "INSERT INTO sink WITH candidate(a) AS (SELECT x FROM source), "
        "stage AS (SELECT c.a AS renamed FROM candidate c), "
        "rollup AS (SELECT COUNT(*) AS n FROM stage) SELECT 1 AS y FROM rollup"
    )
    assert db.raw_connection.execute("SELECT * FROM sink").fetchall() == [(1,)]


def test_joined_columns_with_same_name_remain_distinct():
    db = connection()
    db.execute("CREATE TABLE other(x INTEGER)")
    db.execute("INSERT INTO other VALUES (100)")
    db.register_policy(
        Policy(
            sources=["source", "other"],
            sink="sink",
            on_fail=Resolution.KILL,
            constraint="MIN(source.x) > MIN(other.x) AND sink.y > 0",
        )
    )
    with pytest.raises(Exception, match="KILLing due to dfc policy violation"):
        db.execute(
            "INSERT INTO sink WITH candidate AS (SELECT x FROM source), "
            "other_candidate AS (SELECT x FROM other), "
            "rollup AS (SELECT COUNT(*) AS n FROM candidate CROSS JOIN other_candidate) "
            "SELECT 1 AS y FROM rollup"
        )
    assert db.raw_connection.execute("SELECT * FROM sink").fetchall() == []


def test_computed_column_cannot_impersonate_original_source_column():
    db = connection()
    db.register_policy(policy("MIN(source.x) > 10 AND sink.y > 0"))
    with pytest.raises(Exception, match="cannot uniquely resolve deferred aggregate source column"):
        db.execute(insert("999 AS x"))
    assert db.raw_connection.execute("SELECT * FROM sink").fetchall() == []


def test_equivalent_existing_aggregate_keeps_its_original_output_name():
    db = connection()
    db.register_policy(policy("MIN(source.x) > 0 AND sink.y > 0"))
    db.execute(insert(aggregate="MIN(x) AS existing_min"))
    assert db.raw_connection.execute("SELECT * FROM sink").fetchall() == [(1,)]
