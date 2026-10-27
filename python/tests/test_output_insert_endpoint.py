"""Keep the established aggregate INSERT-output effect at its final endpoint."""

import duckdb
import pytest
from data_flow_control import Policy, Resolution, dfc


def test_insert_aggregate_output_check_uses_final_projection_not_inner_column():
    raw = duckdb.connect()
    raw.execute(
        "CREATE TABLE a(x INT,y INT); INSERT INTO a VALUES(-1,2),(-1,4); CREATE TABLE target(x INT,y INT)"
    )
    with dfc(raw) as conn:
        conn.register_policy(
            Policy(
                sources=["a"], constraint="MAX(a.y) > 0 AND _OUTPUT_.x > 0", on_fail=Resolution.KILL
            )
        )
        template = """INSERT INTO target(x,y)
            WITH candidates AS (SELECT x,y FROM a),
            rollup AS (SELECT x,SUM(y) AS y FROM candidates GROUP BY x)
            SELECT {value} AS x,y FROM rollup"""
        # A negative inner x must not trigger an effect when final output x is 1.
        conn.execute(template.format(value=1))
        assert raw.execute("SELECT * FROM target").fetchall() == [(1, 6)]
        with pytest.raises(duckdb.Error):
            conn.execute(template.format(value=-1)).fetchall()
        assert raw.execute("SELECT * FROM target").fetchall() == [(1, 6)]


def test_partial_push_output_endpoint_gap_is_reported_before_sql_execution():
    from data_flow_control import PassantRewriteError
    raw = duckdb.connect()
    raw.execute("CREATE TABLE a(x INT,y INT); INSERT INTO a VALUES(-1,2)")
    with dfc(raw) as conn:
        conn.register_policy(Policy(sources=["a"], constraint="_OUTPUT_.x > 0 AND MEDIAN(a.y) > 0", on_fail=Resolution.REMOVE))
        with pytest.raises(PassantRewriteError, match="partial-push policy evaluation does not yet bind final _OUTPUT_"):
            conn.transform_query("SELECT 1 AS x FROM a")
        assert conn.execute("SELECT y FROM a").fetchall() == [(2,)]
