"""Aggregate registry introspection and custom UDAF registration."""

import sqlite3

import duckdb
import pytest

from data_flow_control import Policy, Resolution, dfc
from data_flow_control.aggregate_introspection import (
    introspect_duckdb_aggregates,
    introspect_sqlite_aggregates,
    normalize_aggregate_entry,
)
from data_flow_control.catalog import build_catalog_snapshot


def test_build_catalog_snapshot_includes_aggregate_functions():
    snapshot = build_catalog_snapshot(
        dialect="duckdb",
        tables={"foo": {"columns": ["id"], "types": {"id": "INTEGER"}}},
        aggregate_functions=[normalize_aggregate_entry("my_udaf")],
    )
    assert "my_udaf" in {entry["name"] for entry in snapshot["aggregate_functions"]}


def test_duckdb_introspection_includes_median_and_list():
    conn = duckdb.connect()
    try:
        names = {entry["name"].lower() for entry in introspect_duckdb_aggregates(conn)}
        assert "median" in names
        assert "list" in names or "array_agg" in names
        assert "string_agg" in names
    finally:
        conn.close()


def test_duckdb_policy_with_median_registers():
    conn = duckdb.connect()
    try:
        conn.execute("CREATE TABLE foo (id INTEGER, amount DOUBLE)")
        conn.execute("INSERT INTO foo VALUES (1, 10.0)")
        dfc_conn = dfc(conn)
        dfc_conn.refresh_catalog(force=True)
        policy = Policy(
            sources=["foo"],
            constraint="median(foo.amount) > 5",
            on_fail=Resolution.REMOVE,
        )
        dfc_conn.register_policy(policy)
    finally:
        conn.close()


def test_sqlite_custom_aggregate_before_dfc():
    conn = sqlite3.connect(":memory:")
    try:
        conn.execute("CREATE TABLE foo (amount REAL)")
        conn.execute("INSERT INTO foo VALUES (1.0)")

        class MySum:
            def __init__(self):
                self.total = 0.0

            def step(self, value):
                if value is not None:
                    self.total += float(value)

            def finalize(self):
                return self.total

        conn.create_aggregate("mysum", 1, MySum)
        names = {entry["name"].lower() for entry in introspect_sqlite_aggregates(conn)}
        assert "mysum" in names

        dfc_conn = dfc(conn)
        dfc_conn.refresh_catalog(force=True)
        dfc_conn.register_policy(
            Policy(
                sources=["foo"],
                constraint="mysum(foo.amount) > 0",
                on_fail=Resolution.REMOVE,
            )
        )
    finally:
        conn.close()


def test_sqlite_scalar_still_rejects_unaggregated_source():
    conn = sqlite3.connect(":memory:")
    try:
        conn.execute("CREATE TABLE foo (amount REAL)")
        dfc_conn = dfc(conn)
        dfc_conn.refresh_catalog(force=True)
        with pytest.raises(Exception):
            dfc_conn.register_policy(
                Policy(
                    sources=["foo"],
                    constraint="abs(foo.amount) > 0",
                    on_fail=Resolution.REMOVE,
                )
            )
    finally:
        conn.close()


def test_register_aggregate_function_name_override():
    conn = duckdb.connect()
    try:
        conn.execute("CREATE TABLE foo (amount DOUBLE)")
        dfc_conn = dfc(conn)
        dfc_conn.register_aggregate_function_name("custom_agg")
        dfc_conn.refresh_catalog(force=True)
        dfc_conn.register_policy(
            Policy(
                sources=["foo"],
                constraint="custom_agg(foo.amount) > 0",
                on_fail=Resolution.REMOVE,
            )
        )
    finally:
        conn.close()


def test_datafusion_udaf_introspection_optional():
    pytest.importorskip("datafusion")
    from datafusion import SessionContext

    from data_flow_control.adapters.datafusion import DataFusionAdapter

    ctx = SessionContext()
    adapter = DataFusionAdapter(ctx)
    aggregates = adapter.introspect_aggregate_functions()
    names = {entry["name"].lower() for entry in aggregates}
    assert "sum" in names or "count" in names
