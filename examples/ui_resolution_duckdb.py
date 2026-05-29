#!/usr/bin/env python3
"""Manual smoke test for ON FAIL UI resolution with DuckDB."""

from __future__ import annotations

import os
import sys
import tempfile

import duckdb

from data_flow_control import Policy, Resolution, dfc


def main() -> int:
    extension_path = os.environ.get("PASSANT_EXTERNAL_EXTENSION")
    raw = duckdb.connect(config={"allow_unsigned_extensions": "true"} if extension_path else {})
    conn = dfc(raw)
    with tempfile.NamedTemporaryFile(suffix=".tsv", delete=False) as stream:
        stream_path = stream.name

    def handler(event):
        print("UI violation:", event.row)
        if event.row.get("category") == "meal":
            return {"business_use_pct": 40}
        return None

    conn.configure_ui_resolution(
        handler,
        stream_endpoint=stream_path,
        extension_path=extension_path,
    )

    conn.execute(
        "CREATE TABLE bank_txn (txn_id INTEGER, amount INTEGER, category VARCHAR, business_use_pct INTEGER)"
    )
    conn.execute(
        "CREATE TABLE irs_form (txn_id INTEGER, amount INTEGER, category VARCHAR, business_use_pct INTEGER)"
    )
    conn.execute("INSERT INTO bank_txn VALUES (1, 100, 'office', 100), (2, 50, 'meal', 80)")

    conn.register_policy(
        Policy(
            sources=["bank_txn"],
            sink="irs_form",
            constraint="NOT bank_txn.category = 'meal' OR irs_form.business_use_pct <= 50",
            on_fail=Resolution.UI,
            description="Meal expenses require business use <= 50%.",
        )
    )

    if extension_path:
        conn.execute(
            "INSERT INTO irs_form SELECT txn_id, amount, category, business_use_pct FROM bank_txn"
        )
        rows = conn.fetchall(
            "SELECT txn_id, category, business_use_pct FROM irs_form ORDER BY txn_id"
        )
        print("Sink rows:", rows)
    else:
        rewritten = conn.transform_query(
            "INSERT INTO irs_form SELECT txn_id, amount, category, business_use_pct FROM bank_txn"
        )
        print("Rewritten SQL (set PASSANT_EXTERNAL_EXTENSION to execute with stream union):")
        print(rewritten)

    return 0


if __name__ == "__main__":
    sys.exit(main())
