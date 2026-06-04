#!/usr/bin/env python3
"""Compare bulk register_policies() vs per-policy register_policy()."""

from __future__ import annotations

import time

import duckdb

from data_flow_control import Policy, Resolution, dfc


def make_policies(n: int) -> list[Policy]:
    return [
        Policy(
            sources=[f"t_{i:03d}"],
            constraint=f"max(t_{i:03d}.id) > 0",
            on_fail=Resolution.REMOVE,
        )
        for i in range(n)
    ]


def setup(conn) -> None:
    for i in range(50):
        conn.execute(f"CREATE OR REPLACE TABLE t_{i:03d} (id INTEGER)")


def main() -> None:
    n = 50
    policies = make_policies(n)

    conn_loop = dfc(duckdb.connect())
    setup(conn_loop)
    conn_loop.refresh_catalog()
    t0 = time.perf_counter()
    for p in policies:
        conn_loop.register_policy(p)
    loop_s = time.perf_counter() - t0

    conn_bulk = dfc(duckdb.connect())
    setup(conn_bulk)
    conn_bulk.refresh_catalog()
    t1 = time.perf_counter()
    conn_bulk.register_policies(policies)
    bulk_s = time.perf_counter() - t1

    print(f"loop register_policy x{n}: {loop_s:.3f}s")
    print(f"bulk register_policies x{n}: {bulk_s:.3f}s")


if __name__ == "__main__":
    main()
