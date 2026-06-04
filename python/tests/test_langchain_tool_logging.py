"""Tests for direct LangChain tool-call logging middleware."""

from __future__ import annotations

import json

import pytest

pytestmark = pytest.mark.langchain

from langchain.tools import tool
from langchain_core.messages import ToolMessage

from data_flow_control.langchain.database import ToolUseDatabase
from data_flow_control.langchain.middleware import build_passant_tool_middleware
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
def failing_tool(value: str) -> str:
    """Always fails."""
    raise ValueError("boom")


class _FakeRequest:
    def __init__(self, name: str, args: dict, tool_call_id: str = "call-1") -> None:
        self.tool_call = {"name": name, "args": args, "id": tool_call_id}
        self.runtime = None


def _run_middleware(db: ToolUseDatabase, tool_name: str, args: dict, handler_result: str):
    middleware = build_passant_tool_middleware(db)

    def handler(_request):
        return ToolMessage(content=handler_result, tool_call_id="call-1")

    request = _FakeRequest(tool_name, args)
    return middleware.wrap_tool_call(request, handler)


def test_direct_sync_call_logs_input_and_output():
    specs = tools_to_specs([search_customer], output_schemas={"search_customer": CustomerOutput})
    db = ToolUseDatabase(specs)
    try:
        _run_middleware(
            db,
            "search_customer",
            {"customer_id": "c-1"},
            json.dumps({"customer_id": "c-1", "allowed_to_contact": True, "summary": "ok"}),
        )
        inputs = db.fetchall(
            "SELECT customer_id, __passant_origin, __passant_status FROM SearchCustomerInput"
        )
        assert inputs == [("c-1", "direct", "succeeded")]
        outputs = db.fetchall(
            "SELECT __passant_status, customer_id, allowed_to_contact FROM SearchCustomerOutput"
        )
        assert outputs == [("succeeded", "c-1", True)]
    finally:
        db.close()


def test_failed_direct_call_records_error():
    specs = tools_to_specs([failing_tool], output_schemas={"failing_tool": {"value": str}})
    db = ToolUseDatabase(specs)
    try:
        middleware = build_passant_tool_middleware(db)

        def handler(_request):
            raise ValueError("boom")

        request = _FakeRequest("failing_tool", {"value": "x"})
        with pytest.raises(ValueError, match="boom"):
            middleware.wrap_tool_call(request, handler)

        inputs = db.fetchall("SELECT __passant_status, __passant_error FROM FailingToolInput")
        assert inputs[0][0] == "failed"
        outputs = db.fetchall("SELECT __passant_status, __passant_error FROM FailingToolOutput")
        assert outputs[0][0] == "failed"
        assert outputs[0][1] == "boom"
    finally:
        db.close()
