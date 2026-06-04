"""Tests for CallToolWithDataFlow execution."""

from __future__ import annotations

import json

import pytest

pytestmark = pytest.mark.langchain

from langchain.tools import tool

from data_flow_control.langchain.call_tool_with_dataflow import CallToolWithDataFlowRunner
from data_flow_control.langchain.database import ToolUseDatabase
from data_flow_control.langchain.sql_validation import validate_call_tool_sql
from data_flow_control.langchain.tool_spec import tools_to_specs
from langchain_helpers import CustomerOutput


@tool
def search_customer(customer_id: str) -> dict:
    """Search for a customer."""
    return {
        "customer_id": customer_id,
        "allowed_to_contact": True,
        "summary": f"Customer {customer_id}",
    }


@tool
def send_email(customer_id: str, subject: str, body: str) -> dict:
    """Send email."""
    return {"sent": True, "customer_id": customer_id}


@pytest.fixture
def tool_db():
    specs = tools_to_specs(
        [search_customer, send_email],
        output_schemas={
            "search_customer": CustomerOutput,
            "send_email": {"sent": bool, "customer_id": str},
        },
    )
    db = ToolUseDatabase(specs)
    yield db
    db.close()


def test_insert_into_input_executes_tool(tool_db):
    runner = CallToolWithDataFlowRunner(tool_db)
    payload = json.loads(runner.run("INSERT INTO SearchCustomerInput (customer_id) VALUES ('c-1')"))
    assert payload["tool_calls_executed"] == 1
    assert payload["tool_outputs"][0]["status"] == "succeeded"
    outputs = tool_db.fetchall("SELECT customer_id, allowed_to_contact FROM SearchCustomerOutput")
    assert outputs == [("c-1", True)]


def test_select_returns_rows(tool_db):
    runner = CallToolWithDataFlowRunner(tool_db)
    runner.run("INSERT INTO SearchCustomerInput (customer_id) VALUES ('c-2')")
    payload = json.loads(
        runner.run("SELECT customer_id FROM SearchCustomerOutput ORDER BY customer_id")
    )
    assert payload["statement_type"] == "select"
    assert payload["rows"] == [["c-2"]]


def test_rejects_output_table_insert(tool_db):
    with pytest.raises(ValueError, match="not allowed"):
        validate_call_tool_sql(
            "INSERT INTO SearchCustomerOutput (__passant_input_id, __passant_status) VALUES (1, 'succeeded')",
            allowed_input_tables=tool_db.allowed_input_tables,
            protected_tables=tool_db.protected_tables,
        )


def test_rejects_passant_metadata_columns_on_input_insert(tool_db):
    with pytest.raises(ValueError, match="reserved Passant metadata column"):
        validate_call_tool_sql(
            "INSERT INTO SearchCustomerInput (__passant_status, customer_id) VALUES ('succeeded', 'poison')",
            allowed_input_tables=tool_db.allowed_input_tables,
            protected_tables=tool_db.protected_tables,
        )


def test_metadata_status_bypass_does_not_skip_tool_execution(tool_db):
    runner = CallToolWithDataFlowRunner(tool_db)
    with pytest.raises(ValueError, match="reserved Passant metadata column"):
        runner.run(
            "INSERT INTO SearchCustomerInput (__passant_status, customer_id) "
            "VALUES ('succeeded', 'poison')"
        )
    assert tool_db.fetchall("SELECT COUNT(*) FROM SearchCustomerOutput")[0][0] == 0


def test_rejects_insert_without_explicit_column_list(tool_db):
    with pytest.raises(ValueError, match="explicit non-metadata column list"):
        validate_call_tool_sql(
            "INSERT INTO SearchCustomerInput VALUES ('c-1')",
            allowed_input_tables=tool_db.allowed_input_tables,
            protected_tables=tool_db.protected_tables,
        )


def test_fetchall_rejects_non_select(tool_db):
    with pytest.raises(ValueError, match="fetchall only supports SELECT"):
        tool_db.fetchall("INSERT INTO SearchCustomerInput (customer_id) VALUES ('x')")


def test_rejects_multiple_statements(tool_db):
    with pytest.raises(ValueError, match="single SQL statement"):
        validate_call_tool_sql(
            "SELECT 1; SELECT 2",
            allowed_input_tables=tool_db.allowed_input_tables,
            protected_tables=tool_db.protected_tables,
        )


def test_pending_rows_execute_exactly_once(tool_db):
    runner = CallToolWithDataFlowRunner(tool_db)
    first = json.loads(
        runner.run("INSERT INTO SearchCustomerInput (customer_id) VALUES ('pending-1')")
    )
    assert first["tool_calls_executed"] == 1
    second = json.loads(runner.run("SELECT 1"))
    assert second["tool_calls_executed"] == 0


def test_tool_error_recorded(tool_db):
    @tool
    def broken_tool(value: str) -> str:
        """Broken."""
        raise RuntimeError("tool failed")

    specs = tools_to_specs([broken_tool], output_schemas={"broken_tool": {"value": str}})
    db = ToolUseDatabase(specs)
    try:
        runner = CallToolWithDataFlowRunner(db)
        payload = json.loads(runner.run("INSERT INTO BrokenToolInput (value) VALUES ('x')"))
        assert payload["tool_outputs"][0]["status"] == "failed"
        row = db.fetchall("SELECT __passant_status, __passant_error FROM BrokenToolOutput")[0]
        assert row[0] == "failed"
        assert row[1] == "tool failed"
    finally:
        db.close()
