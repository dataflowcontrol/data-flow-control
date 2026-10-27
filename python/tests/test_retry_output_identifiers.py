"""Abort-stage output references retain quoted public identifiers."""

import duckdb
import pytest
from data_flow_control import Policy, Resolution, dfc


@pytest.mark.parametrize("operator", ["UNION", "UNION ALL"])
@pytest.mark.parametrize("label", ["A B", "MiXeD", "select", 'a"b'])
def test_retry_union_keeps_quoted_output_identifier(operator, label):
    raw = duckdb.connect()
    raw.execute("CREATE TABLE a(x INTEGER); CREATE TABLE b(x INTEGER)")
    raw.execute("INSERT INTO a VALUES (1), (1); INSERT INTO b VALUES (2)")
    conn = dfc(raw)
    try:
        conn.register_policy(
            Policy(
                sources=["a"],
                constraint="MAX(a.x) > 0",
                on_fail=Resolution.RETRY,
                retry_message="positive required",
            )
        )
        quoted = '"' + label.replace('"', '""') + '"'
        cursor = conn.execute(f"SELECT x AS {quoted} FROM a {operator} SELECT x FROM b")
        assert [column[0] for column in cursor.description] == [label]
        assert sorted(cursor.fetchall()) == (
            [(1,), (2,)] if operator == "UNION" else [(1,), (1,), (2,)]
        )
    finally:
        conn.close()


@pytest.mark.parametrize("operator", ["UNION", "UNION ALL"])
@pytest.mark.parametrize("projection", ["7", "x + 1"])
def test_retry_union_keeps_implicit_output_label(operator, projection):
    raw = duckdb.connect()
    raw.execute("CREATE TABLE a(x INTEGER); CREATE TABLE b(x INTEGER)")
    raw.execute("INSERT INTO a VALUES (1), (1); INSERT INTO b VALUES (2)")
    query = f"SELECT {projection} FROM a {operator} SELECT {projection} FROM b"
    baseline = raw.execute(query)
    expected_names = [column[0] for column in baseline.description]
    expected_rows = sorted(baseline.fetchall())
    conn = dfc(raw)
    try:
        conn.register_policy(
            Policy(
                sources=["a"],
                constraint="MAX(a.x) > 0",
                on_fail=Resolution.RETRY,
                retry_message="positive required",
            )
        )
        cursor = conn.execute(query)
        assert [column[0] for column in cursor.description] == expected_names
        assert sorted(cursor.fetchall()) == expected_rows
    finally:
        conn.close()
