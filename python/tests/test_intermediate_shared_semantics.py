"""The live-SQL and opaque-state paths share policy applicability semantics."""

import duckdb
import pytest

from data_flow_control import Policy, dfc


@pytest.mark.parametrize("propagate", [False, True])
@pytest.mark.parametrize(
    "query,expected", [("SELECT x FROM a", [(1,)]), ("SELECT x AS y FROM a", [])]
)
def test_missing_output_column_deactivates_policy(propagate, query, expected):
    with dfc(duckdb.connect()) as conn:
        conn.execute("CREATE TABLE a(x INTEGER)")
        conn.execute("INSERT INTO a VALUES (1)")
        conn.register_policy(Policy.from_pgn("SOURCE a CONSTRAINT _OUTPUT_.y > 10 ON FAIL REMOVE"))
        if propagate:
            conn.enable_intermediate_propagation()
        assert conn.fetchall(query) == expected


@pytest.mark.parametrize("propagate", [False, True])
@pytest.mark.parametrize("ordinary_present", [False, True])
def test_missing_ordinary_source_precedes_required_source(propagate, ordinary_present):
    with dfc(duckdb.connect()) as conn:
        for table in ("a", "b", "updates"):
            conn.execute(f"CREATE TABLE {table}(id INTEGER)")
        conn.execute("INSERT INTO b VALUES (1)")
        conn.register_policy(
            Policy.from_pgn("SOURCE REQUIRED a, b SINK updates CONSTRAINT a.id=b.id ON FAIL REMOVE")
        )
        if propagate:
            conn.enable_intermediate_propagation()
        conn.execute(
            "INSERT INTO updates " + ("SELECT id FROM b" if ordinary_present else "SELECT 1")
        )
        assert conn.fetchall("SELECT * FROM updates") == ([] if ordinary_present else [(1,)])


@pytest.mark.parametrize("propagate", [False, True])
@pytest.mark.parametrize(
    "query,expected",
    [
        ("SELECT x FROM a ORDER BY x", [(-1,), (1,)]),
        ("SELECT y FROM b ORDER BY y", [(-1,), (1,)]),
        ("SELECT x,y FROM a CROSS JOIN b ORDER BY x,y", [(1, 1)]),
    ],
)
def test_source_only_conjunction_requires_both_ordinary_sources(propagate, query, expected):
    with dfc(duckdb.connect()) as conn:
        conn.execute("CREATE TABLE a(x INTEGER)")
        conn.execute("CREATE TABLE b(y INTEGER)")
        conn.execute("INSERT INTO a VALUES (-1),(1)")
        conn.execute("INSERT INTO b VALUES (-1),(1)")
        conn.register_policy(
            Policy.from_pgn("SOURCE a,b CONSTRAINT a.x>0 AND b.y>0 ON FAIL REMOVE")
        )
        if propagate:
            conn.enable_intermediate_propagation()
        # Internal partial-source discovery does not make a top-level policy
        # applicable without its ordinary sources. Both paths enforce it once
        # the query actually combines contributors from a and b.
        assert conn.fetchall(query) == expected
