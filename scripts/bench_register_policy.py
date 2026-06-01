#!/usr/bin/env python3
"""Report wall time for repeated register_policy() with catalog refresh behavior."""

from __future__ import annotations

import argparse
import time

import duckdb

from data_flow_control import Policy, Resolution, dfc


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("-n", type=int, default=50, help="policies to register")
    parser.add_argument(
        "--refresh-each",
        action="store_true",
        help="force catalog refresh before every policy (legacy behavior)",
    )
    args = parser.parse_args()

    conn = dfc(duckdb.connect())
    conn.execute("CREATE OR REPLACE TABLE orders (id INTEGER, amount DOUBLE)")
    for i in range(100):
        conn.execute(f"CREATE OR REPLACE TABLE bench_t_{i:03d} (id INTEGER, amount DOUBLE)")

    policies = [
        Policy(
            sources=[f"bench_t_{i:03d}"],
            constraint=f"max(bench_t_{i:03d}.amount) > 0",
            on_fail=Resolution.REMOVE,
        )
        for i in range(args.n)
    ]

    if args.refresh_each:
        start = time.perf_counter()
        for policy in policies:
            conn.refresh_catalog(force=True)
            conn.register_policy(policy)
        elapsed = time.perf_counter() - start
        print(f"register_policy x{args.n} (refresh each): {elapsed:.3f}s")
    else:
        conn.refresh_catalog()
        start = time.perf_counter()
        conn.register_policies(policies)
        elapsed = time.perf_counter() - start
        print(f"register_policies x{args.n} (one catalog sync): {elapsed:.3f}s")


if __name__ == "__main__":
    main()
