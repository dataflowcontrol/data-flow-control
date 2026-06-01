#!/usr/bin/env python3
"""Wall time and rewritten SQL size for execution benchmark fixtures."""

from __future__ import annotations

import pathlib
import time

import duckdb

from data_flow_control import Policy, Resolution, dfc

FIXTURES = pathlib.Path(__file__).resolve().parents[1] / "benchmarks" / "execution"


def timed(label: str, fn) -> None:
    start = time.perf_counter()
    fn()
    print(f"{label}: {time.perf_counter() - start:.4f}s")


def bench_fixture(name: str, sql: str, policies: list[Policy]) -> None:
    conn = dfc(duckdb.connect())
    conn.execute("CREATE OR REPLACE TABLE orders (id INTEGER, amount DOUBLE)")
    conn.execute("CREATE OR REPLACE TABLE expenses (amount DOUBLE)")
    conn.execute("CREATE OR REPLACE TABLE reports (amount DOUBLE)")
    conn.execute("INSERT INTO orders VALUES (1, 10.0), (2, 20.0)")
    conn.execute("INSERT INTO expenses VALUES (40.0), (50.0)")
    for policy in policies:
        conn.register_policy(policy)
    rewritten = conn.transform_query(sql)
    print(f"\n=== {name} ===")
    print(f"rewritten bytes: {len(rewritten)}")
    timed(f"{name} execute", lambda: conn.execute(rewritten).fetchall())


def main() -> None:
    timed(
        "no policies",
        lambda: dfc(duckdb.connect())
        .execute("SELECT id, amount FROM orders WHERE amount > 0")
        .fetchall(),
    )

    conn_many = dfc(duckdb.connect())
    conn_many.execute("CREATE OR REPLACE TABLE orders (id INTEGER, amount DOUBLE)")
    conn_many.execute("INSERT INTO orders VALUES (1, 10.0)")
    for i in range(200):
        conn_many.execute(f"CREATE OR REPLACE TABLE other_{i:03d} (id INTEGER)")
        conn_many.register_policy(
            Policy(
                sources=[f"other_{i:03d}"],
                constraint=f"max(other_{i:03d}.id) > 0",
                on_fail=Resolution.REMOVE,
            )
        )
    timed(
        "no candidates (200 unrelated policies)",
        lambda: conn_many.execute("SELECT id, amount FROM orders WHERE amount > 0").fetchall(),
    )

    conn_one = dfc(duckdb.connect())
    conn_one.execute("CREATE OR REPLACE TABLE orders (id INTEGER, amount DOUBLE)")
    conn_one.execute("INSERT INTO orders VALUES (1, 10.0)")
    conn_one.register_policy(
        Policy(
            sources=["orders"],
            constraint="max(orders.amount) > 5",
            on_fail=Resolution.REMOVE,
        )
    )
    timed(
        "one candidate",
        lambda: conn_one.execute("SELECT id, amount FROM orders WHERE amount > 0").fetchall(),
    )

    scan_sql = (FIXTURES / "scan_remove.sql").read_text()
    bench_fixture(
        "scan_remove",
        scan_sql,
        [
            Policy(
                sources=["orders"],
                constraint="max(orders.amount) > 5",
                on_fail=Resolution.REMOVE,
            )
        ],
    )

    kill_sql = (FIXTURES / "kill_scan.sql").read_text()
    bench_fixture(
        "kill_scan",
        kill_sql,
        [
            Policy(
                sources=["orders"],
                constraint="max(orders.amount) > 5",
                on_fail=Resolution.KILL,
            )
        ],
    )


if __name__ == "__main__":
    main()
