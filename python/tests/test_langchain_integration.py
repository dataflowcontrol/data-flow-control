"""Integration tests for LangChain agent wrapping."""

from __future__ import annotations

import asyncio
import json

import pytest

pytestmark = pytest.mark.langchain

from langchain.tools import tool
from langchain_core.messages import AIMessage, HumanMessage, ToolMessage

from langchain_helpers import CustomerOutput, ScriptableToolChatModel, wrap_tools


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


def test_observe_mode_logs_direct_tool_call():
    model = ScriptableToolChatModel(
        responses=[
            AIMessage(
                content="",
                tool_calls=[
                    {
                        "name": "search_customer",
                        "args": {"customer_id": "c-100"},
                        "id": "call-1",
                    }
                ],
            ),
            AIMessage(content="done"),
        ]
    )
    tools = [search_customer]
    wrapped = wrap_tools(
        model=model,
        tools=tools,
        output_schemas={"search_customer": CustomerOutput},
        direct_tool_mode="observe",
    )
    try:
        wrapped.invoke({"messages": [HumanMessage(content="lookup customer")]})
        rows = wrapped.fetchall(
            "SELECT customer_id, __passant_origin, __passant_status FROM SearchCustomerInput"
        )
        assert rows == [("c-100", "direct", "succeeded")]
    finally:
        wrapped.close()


def test_observe_mode_logs_direct_tool_call_async():
    async def run_async() -> None:
        model = ScriptableToolChatModel(
            responses=[
                AIMessage(
                    content="",
                    tool_calls=[
                        {
                            "name": "search_customer",
                            "args": {"customer_id": "c-async"},
                            "id": "call-1",
                        }
                    ],
                ),
                AIMessage(content="done"),
            ]
        )
        tools = [search_customer]
        wrapped = wrap_tools(
            model=model,
            tools=tools,
            output_schemas={"search_customer": CustomerOutput},
            direct_tool_mode="observe",
        )
        try:
            await wrapped.ainvoke({"messages": [HumanMessage(content="lookup customer")]})
            rows = wrapped.fetchall(
                "SELECT customer_id, __passant_origin, __passant_status FROM SearchCustomerInput"
            )
            assert rows == [("c-async", "direct", "succeeded")]
        finally:
            wrapped.close()

    asyncio.run(run_async())


def test_agent_can_call_call_tool_with_dataflow():
    sql = "INSERT INTO SearchCustomerInput (customer_id) VALUES ('c-200')"
    model = ScriptableToolChatModel(
        responses=[
            AIMessage(
                content="",
                tool_calls=[
                    {
                        "name": "CallToolWithDataFlow",
                        "args": {"sql": sql},
                        "id": "call-1",
                    }
                ],
            ),
            AIMessage(content="done"),
        ]
    )
    tools = [search_customer]
    wrapped = wrap_tools(
        model=model,
        tools=tools,
        output_schemas={"search_customer": CustomerOutput},
        direct_tool_mode="observe",
    )
    try:
        result = wrapped.invoke({"messages": [HumanMessage(content="lookup customer")]})
        tool_messages = [m for m in result["messages"] if isinstance(m, ToolMessage)]
        assert tool_messages
        payload = json.loads(tool_messages[0].content)
        assert payload["tool_calls_executed"] == 1
        rows = wrapped.fetchall("SELECT customer_id FROM SearchCustomerOutput ORDER BY customer_id")
        assert rows == [("c-200",)]
    finally:
        wrapped.close()
