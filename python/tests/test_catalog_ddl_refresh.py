"""Catalog cache must pick up tables created after an earlier policy registration."""

import duckdb

from data_flow_control import Policy, Resolution, dfc


def test_register_policy_after_create_table_without_manual_refresh():
    conn = dfc(duckdb.connect())
    conn.execute("CREATE OR REPLACE TABLE first_tbl (id INTEGER)")
    conn.register_policy(
        Policy(
            sources=["first_tbl"],
            constraint="max(first_tbl.id) > 0",
            on_fail=Resolution.REMOVE,
        )
    )

    conn.execute("CREATE OR REPLACE TABLE second_tbl (id INTEGER)")
    conn.register_policy(
        Policy(
            sources=["second_tbl"],
            constraint="max(second_tbl.id) > 0",
            on_fail=Resolution.REMOVE,
        )
    )

    assert len(conn.policies()) == 2
