#!/usr/bin/env python3
"""Install smoke test: import data-flow-control + duckdb and enforce a REMOVE policy."""

from __future__ import annotations

import sys


def main() -> int:
    import duckdb
    from data_flow_control import Policy, Resolution, dfc

    conn = dfc(duckdb.connect())
    conn.execute("CREATE TABLE foo (id INTEGER)")
    conn.execute("INSERT INTO foo VALUES (1), (2)")

    without_policy = conn.fetchall("SELECT id FROM foo ORDER BY id")
    if without_policy != [(1,), (2,)]:
        print(f"unexpected baseline rows: {without_policy!r}", file=sys.stderr)
        return 1

    conn.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    with_policy = conn.fetchall("SELECT id FROM foo ORDER BY id")
    if with_policy != [(2,)]:
        print(
            f"policy did not filter rows (expected [(2,)], got {with_policy!r})",
            file=sys.stderr,
        )
        return 1

    print("ok: data-flow-control and duckdb installed; REMOVE policy enforced")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
