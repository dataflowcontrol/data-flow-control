"""Tests for Passant LangChain tool table DDL."""

from __future__ import annotations

import pytest

pytestmark = pytest.mark.langchain

from langchain.tools import tool

from data_flow_control.langchain.database import ToolUseDatabase
from data_flow_control.langchain.tool_spec import tools_to_specs
from langchain_helpers import CustomerOutput


def _table_columns(db: ToolUseDatabase, table_name: str) -> list[tuple[str, str]]:
    rows = db._fetchall_raw(f"PRAGMA table_info('{table_name}')")
    return [(row[1], row[2]) for row in rows]


@tool
def search_customer(customer_id: str, include_history: bool = False) -> dict:
    """Search for a customer."""
    return {
        "customer_id": customer_id,
        "allowed_to_contact": True,
        "summary": "ok",
    }


@tool
def send_email(customer_id: str, subject: str, body: str) -> dict:
    """Send an email."""
    return {"sent": True}


def test_registry_and_tool_tables_created():
    specs = tools_to_specs(
        [search_customer, send_email],
        output_schemas={
            "search_customer": CustomerOutput,
            "send_email": {"sent": bool},
        },
    )
    db = ToolUseDatabase(specs)
    try:
        registry = db.fetchall(
            "SELECT tool_name, input_table, output_table FROM __passant_tool_registry ORDER BY tool_name"
        )
        assert registry == [
            ("search_customer", "SearchCustomerInput", "SearchCustomerOutput"),
            ("send_email", "SendEmailInput", "SendEmailOutput"),
        ]

        input_columns = _table_columns(db, "SearchCustomerInput")
        assert ("__passant_input_id", "BIGINT") in input_columns
        assert ("__passant_status", "VARCHAR") in input_columns
        assert ("customer_id", "VARCHAR") in input_columns
        assert ("include_history", "BOOLEAN") in input_columns

        output_columns = _table_columns(db, "SearchCustomerOutput")
        assert ("__passant_output_id", "BIGINT") in output_columns
        assert ("__passant_raw_json", "JSON") in output_columns
        assert ("allowed_to_contact", "BOOLEAN") in output_columns
    finally:
        db.close()


def test_catalog_refresh_allows_policy_registration():
    from data_flow_control import Policy, Resolution

    specs = tools_to_specs(
        [search_customer, send_email],
        output_schemas={
            "search_customer": CustomerOutput,
            "send_email": {"sent": bool},
        },
    )
    db = ToolUseDatabase(
        specs,
        policies=[
            Policy(
                sources=["SearchCustomerOutput"],
                required_sources=["SearchCustomerOutput"],
                sink="SendEmailInput",
                constraint=(
                    "max(SearchCustomerOutput.customer_id) = SendEmailInput.customer_id "
                    "AND bool_or(SearchCustomerOutput.allowed_to_contact)"
                ),
                on_fail=Resolution.REMOVE,
            )
        ],
    )
    try:
        assert len(db.conn.policies()) == 1
    finally:
        db.close()
