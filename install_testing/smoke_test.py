#!/usr/bin/env python3
"""Exercise REMOVE, KILL, and INSERT using an installed distribution."""

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

    conn.close()

    conn = dfc(duckdb.connect())
    conn.execute("CREATE TABLE receipts (amount INTEGER)")
    conn.execute("INSERT INTO receipts VALUES (10), (20)")
    conn.execute("CREATE TABLE expenses (amount INTEGER)")
    conn.register_policy(
        Policy(
            sources=["receipts"],
            sink="expenses",
            constraint="MAX(receipts.amount) <= 20 AND expenses.amount <= 20",
            on_fail=Resolution.KILL,
        )
    )
    conn.execute("INSERT INTO expenses SELECT amount FROM receipts")
    assert conn.raw_connection.execute(
        "SELECT amount FROM expenses ORDER BY amount"
    ).fetchall() == [(10,), (20,)]
    try:
        conn.execute("INSERT INTO expenses SELECT amount + 100 FROM receipts")
    except Exception as exc:
        if "KILLing due to dfc policy violation" not in str(exc):
            raise
    else:
        raise AssertionError("KILL failed to abort a violating INSERT")
    assert conn.raw_connection.execute(
        "SELECT amount FROM expenses ORDER BY amount"
    ).fetchall() == [(10,), (20,)], "violating INSERT must not partially write rows"
    conn.close()

    conn = dfc(duckdb.connect())
    conn.execute("CREATE TABLE items (id INTEGER)")
    conn.execute("INSERT INTO items VALUES (1), (2)")
    conn.register_policy(
        Policy(
            sources=["items"],
            constraint="MAX(items.id) > 1",
            on_fail=Resolution.KILL,
        )
    )
    assert conn.fetchall("SELECT id FROM items WHERE id = 2") == [(2,)]
    try:
        conn.fetchall("SELECT id FROM items")
    except Exception as exc:
        if "KILLing due to dfc policy violation" not in str(exc):
            raise
    else:
        raise AssertionError("KILL failed to abort a violating SELECT")
    conn.close()
    print("ok: installed package enforces REMOVE, KILL, and INSERT policies")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
