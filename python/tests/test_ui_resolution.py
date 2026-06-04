"""Tests for ON FAIL UI resolution."""

from __future__ import annotations

import json
import tempfile

import duckdb
import pytest

from data_flow_control import Policy, Resolution, dfc
from data_flow_control.connection import _statement_needs_ui_stream_union
from data_flow_control.ui import (
    build_ui_violation_event,
    merge_handler_row,
    write_ui_stream_row,
)


def test_resolution_from_label_ui():
    assert Resolution.from_label("UI") == Resolution.UI


def test_policy_from_pgn_ui():
    policy = Policy.from_pgn(
        "SOURCE bank_txn\nSINK irs_form\nCONSTRAINT NOT bank_txn.category = 'meal'\nON FAIL UI"
    )
    assert policy.on_fail == Resolution.UI


def test_register_ui_rejected_without_capability():
    from dataclasses import dataclass

    from data_flow_control.adapters.base import Capabilities
    from data_flow_control.catalog import build_catalog_snapshot
    from data_flow_control.connection import Connection
    from data_flow_control.planner import Planner

    @dataclass
    class _NoUiAdapter:
        dialect: str = "test"
        capabilities: Capabilities = Capabilities()

        def execute(self, sql: str, params=None):
            return duckdb.connect().execute(sql, params)

        def introspect_aggregate_functions(self) -> list[dict]:
            return []

        def introspect_catalog(self) -> dict:
            return build_catalog_snapshot(
                dialect=self.dialect,
                tables={"foo": {"columns": ["id"], "types": {"id": "INTEGER"}}},
            )

        def quote_identifier(self, name: str) -> str:
            return name

        def register_kill_function(self) -> None:
            pass

        def register_resolution_function(self, name, func, parameter_types, return_type) -> None:
            pass

        def register_relation_resolution_function(self, name, func) -> None:
            pass

        def close(self) -> None:
            pass

    db = Connection(_NoUiAdapter(), planner=Planner(dialect="test"))
    with pytest.raises(ValueError, match="ui_resolution"):
        db.register_policy(Policy(sources=["foo"], constraint="foo.id > 0", on_fail=Resolution.UI))


def test_configure_without_extension_allows_rewrite_only():
    conn = dfc(duckdb.connect())
    conn.configure_ui_resolution(lambda _event: None)
    assert conn._ui_stream_extension_ready is False


def test_statement_needs_ui_stream_union_with_cte_select():
    sql = (
        "WITH t AS (SELECT 1 AS id) "
        "SELECT id FROM t WHERE address_violating_rows(id, 'c', '', '[]', '/tmp/x.tsv')"
    )
    assert _statement_needs_ui_stream_union(sql) is True


def test_statement_needs_ui_stream_union_update_edited_is_false():
    sql = (
        "UPDATE bar SET amount = 50 WHERE CASE WHEN bar.amount <= 100 THEN true "
        "ELSE address_violating_rows(bar.amount, 'c', '', '[]', '/tmp/x.tsv') END"
    )
    assert _statement_needs_ui_stream_union(sql) is False


def test_execute_with_select_ui_requires_stream_extension():
    conn = dfc(duckdb.connect())
    conn.execute("CREATE TABLE foo (id INTEGER)")
    with tempfile.NamedTemporaryFile(suffix=".tsv") as stream:
        conn.configure_ui_resolution(lambda _event: None, stream_endpoint=stream.name)
        conn.register_policy(
            Policy(
                sources=["foo"],
                constraint="foo.id > 0",
                on_fail=Resolution.UI,
            )
        )
        with pytest.raises(ValueError, match="extended_duckdb"):
            conn.execute("WITH t AS (SELECT id FROM foo) SELECT id FROM foo")


def test_execute_insert_ui_requires_stream_extension():
    conn = dfc(duckdb.connect())
    conn.execute("CREATE TABLE foo (id INTEGER)")
    conn.execute("CREATE TABLE bar (id INTEGER)")
    with tempfile.NamedTemporaryFile(suffix=".tsv") as stream:
        conn.configure_ui_resolution(lambda _event: None, stream_endpoint=stream.name)
        conn.register_policy(
            Policy(
                sources=["foo"],
                sink="bar",
                constraint="foo.id > 0",
                on_fail=Resolution.UI,
            )
        )
        with pytest.raises(ValueError, match="extended_duckdb"):
            conn.execute("INSERT INTO bar SELECT id FROM foo")


def test_duckdb_rejects_ui_without_configure():
    conn = dfc(duckdb.connect())
    with pytest.raises(ValueError, match="configure_ui_resolution"):
        conn.register_policy(
            Policy(
                sources=["foo"],
                sink="bar",
                constraint="foo.id > 0",
                on_fail=Resolution.UI,
            )
        )


def test_duckdb_ui_rewrite_includes_stream_endpoint():
    conn = dfc(duckdb.connect())
    conn.execute("CREATE TABLE foo (id INTEGER)")
    conn.execute("CREATE TABLE bar (id INTEGER)")
    with tempfile.NamedTemporaryFile(suffix=".tsv") as stream:
        conn.configure_ui_resolution(lambda _event: None, stream_endpoint=stream.name)
        conn.register_policy(
            Policy(
                sources=["foo"],
                sink="bar",
                constraint="foo.id > 0",
                on_fail=Resolution.UI,
            )
        )
        rewritten = conn.transform_query("INSERT INTO bar SELECT id FROM foo")
    assert "address_violating_rows" in rewritten
    assert stream.name in rewritten
    assert "t1 AS" not in rewritten


def test_ui_violation_event_parsing():
    event = build_ui_violation_event(
        (
            1,
            "meal",
            "NOT x",
            "desc",
            json.dumps(["foo.id", "category"]),
            "/tmp/stream.tsv",
        )
    )
    assert event.column_names == ["foo.id", "category"]
    assert event.source_columns == ["foo.id"]
    assert event.output_columns == ["category"]
    assert event.values == [1, "meal"]


def test_handler_none_drops_row_via_udf_return():
    conn = dfc(duckdb.connect())
    with tempfile.NamedTemporaryFile(suffix=".tsv") as stream:
        conn.configure_ui_resolution(lambda _event: None, stream_endpoint=stream.name)
        result = conn.raw_connection.execute(
            "SELECT address_violating_rows(1, 'constraint', 'desc', '[\"x\"]', ?)",
            [stream.name],
        ).fetchone()
    assert result == (False,)


def test_handler_dict_writes_stream_row():
    with tempfile.NamedTemporaryFile(suffix=".tsv", delete=False) as stream:
        path = stream.name
    write_ui_stream_row(path, ["a", "b"], [1, 2])
    with open(path, encoding="utf-8") as handle:
        assert handle.read() == "1\t2\n"


def test_merge_handler_row_preserves_unmentioned_columns():
    event = build_ui_violation_event((10, 20, "c", "", json.dumps(["s.x", "out"]), "/tmp/s.tsv"))
    merged = merge_handler_row(event, {"out": 99})
    assert merged == [10, 99]


def test_standalone_select_emits_ui_filter():
    conn = dfc(duckdb.connect())
    conn.execute("CREATE TABLE foo (id INTEGER, label VARCHAR)")
    with tempfile.NamedTemporaryFile(suffix=".tsv") as stream:
        conn.configure_ui_resolution(lambda _event: None, stream_endpoint=stream.name)
        conn.register_policy(
            Policy(
                sources=["foo"],
                constraint="foo.id > 0",
                on_fail=Resolution.UI,
            )
        )
        rewritten = conn.transform_query("SELECT id, label FROM foo")
    assert "address_violating_rows" in rewritten
    assert "t1 AS" not in rewritten


def test_update_approval_emits_passant_ui_approve():
    conn = dfc(duckdb.connect())
    conn.execute("CREATE TABLE bar (id INTEGER PRIMARY KEY, amount INTEGER)")
    with tempfile.NamedTemporaryFile(suffix=".tsv") as stream:
        conn.configure_ui_resolution(
            lambda _event: {"amount": 99},
            stream_endpoint=stream.name,
            update_mode="approval_only",
        )
        conn.register_policy(
            Policy(
                sources=[],
                sink="bar",
                constraint="bar.amount > 100",
                on_fail=Resolution.UI,
            )
        )
        rewritten = conn.transform_query("UPDATE bar SET amount = 50 WHERE id = 1")
    assert "passant_ui_approve" in rewritten


def test_update_edited_produces_followup_sql():
    conn = dfc(duckdb.connect())
    conn.execute("CREATE TABLE bar (id INTEGER PRIMARY KEY, amount INTEGER)")
    with tempfile.NamedTemporaryFile(suffix=".tsv") as stream:
        conn.configure_ui_resolution(
            lambda _event: {"amount": 42},
            stream_endpoint=stream.name,
            update_mode="edited_rows",
        )
        conn.register_policy(
            Policy(
                sources=[],
                sink="bar",
                constraint="bar.amount > 100",
                on_fail=Resolution.UI,
            )
        )
        conn.transform_query("UPDATE bar SET amount = 50 WHERE id = 1")
        followup = conn.planner.last_ui_followup_sql()
    assert followup is not None
    assert "read_csv" in followup
    assert "staged" in followup
    assert "SET amount = staged.amount" in followup
    assert "bar.amount = staged.amount" not in followup
    assert "names=['id', 'amount']" in followup


def test_update_edited_followup_executes_against_staged_stream():
    conn = dfc(duckdb.connect())
    conn.execute("CREATE TABLE bar (id INTEGER PRIMARY KEY, amount INTEGER)")
    conn.execute("INSERT INTO bar VALUES (1, 200)")
    with tempfile.NamedTemporaryFile(suffix=".tsv", delete=False) as stream:
        stream_path = stream.name
    conn.configure_ui_resolution(
        lambda _event: {"amount": 42},
        stream_endpoint=stream_path,
        update_mode="edited_rows",
    )
    write_ui_stream_row(stream_path, ["id", "amount"], [1, 42])
    conn.register_policy(
        Policy(
            sources=[],
            sink="bar",
            constraint="bar.amount > 100",
            on_fail=Resolution.UI,
        )
    )
    conn.transform_query("UPDATE bar SET amount = 50 WHERE id = 1")
    followup = conn.planner.last_ui_followup_sql()
    assert followup is not None
    conn.raw_connection.execute(followup)
    assert conn.fetchall("SELECT id, amount FROM bar ORDER BY id") == [(1, 42)]
