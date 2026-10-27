"""Repeated output aliases are not repeated scans; real self-joins remain guarded."""

import duckdb
import pytest
from data_flow_control import PassantRewriteError, Policy, Resolution, dfc


def test_derived_duplicate_metric_projection_keeps_single_scan_identity():
    raw = duckdb.connect()
    raw.execute("CREATE TABLE lineitem(l_orderkey INTEGER,l_quantity INTEGER)")
    raw.execute("INSERT INTO lineitem VALUES (1,2),(1,3),(2,-2),(2,-1)")
    with dfc(raw) as conn:
        conn.register_policy(
            Policy(
                sources=["lineitem"],
                constraint="MAX(lineitem.l_quantity) >= 1",
                on_fail=Resolution.REMOVE,
            )
        )
        cursor = conn.execute(
            "SELECT g, SUM(v) AS total FROM "
            "(SELECT lineitem.l_quantity AS q, l_quantity AS v, l_orderkey AS g FROM lineitem) AS d GROUP BY g"
        )
        assert [column[0] for column in cursor.description] == ["g", "total"]
        assert cursor.fetchall() == [(1, 5)]


@pytest.mark.parametrize("use_cte", [False, True])
def test_true_repeated_source_occurrences_remain_explicitly_unsupported(use_cte):
    raw = duckdb.connect()
    raw.execute("CREATE TABLE lineitem(l_orderkey INTEGER,l_quantity INTEGER)")
    raw.execute("INSERT INTO lineitem VALUES (1,2)")
    with dfc(raw) as conn:
        conn.register_policy(
            Policy(
                sources=["lineitem"],
                constraint="MAX(lineitem.l_quantity) >= 1",
                on_fail=Resolution.RETRY,
                retry_message="needs unique occurrence",
            )
        )
        if use_cte:
            query = "WITH d AS (SELECT l_orderkey,l_quantity FROM lineitem) SELECT a.l_quantity FROM d AS a JOIN d AS b ON a.l_orderkey=b.l_orderkey"
        else:
            query = "SELECT q FROM (SELECT a.l_quantity AS q FROM lineitem AS a JOIN lineitem AS b ON a.l_orderkey=b.l_orderkey) AS d"
        with pytest.raises(PassantRewriteError, match="multiple occurrences"):
            conn.execute(query)
