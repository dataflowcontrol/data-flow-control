"""End-to-end UI resolution tests with extended_duckdb (optional)."""

from __future__ import annotations

import tempfile

import duckdb
import pytest

from data_flow_control import Policy, Resolution, dfc
from ui_extension_support import ui_extension_available, ui_extension_path


@pytest.mark.ui_extension
@pytest.mark.skipif(not ui_extension_available(), reason="extended_duckdb extension not built")
def test_insert_ui_union_corrected_row_into_sink():
    extension_path = ui_extension_path()
    assert extension_path is not None
    conn = dfc(duckdb.connect(config={"allow_unsigned_extensions": "true"}))
    with tempfile.NamedTemporaryFile(suffix=".tsv", delete=False) as stream:
        stream_path = stream.name

    def handler(event):
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
        )
    )
    conn.execute(
        "INSERT INTO irs_form SELECT txn_id, amount, category, business_use_pct FROM bank_txn"
    )
    rows = conn.fetchall("SELECT txn_id, category, business_use_pct FROM irs_form ORDER BY txn_id")
    assert rows == [(1, "office", 100), (2, "meal", 40)]
