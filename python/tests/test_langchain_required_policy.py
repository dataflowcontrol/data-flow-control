"""Tests for required-source policies through LangChain tool tables."""

from __future__ import annotations

import json

import pytest

pytestmark = pytest.mark.langchain

from langchain.tools import tool

from data_flow_control import Policy, Resolution
from data_flow_control.langchain.call_tool_with_dataflow import CallToolWithDataFlowRunner
from data_flow_control.langchain.database import ToolUseDatabase
from data_flow_control.langchain.tool_spec import tools_to_specs
from langchain_helpers import CustomerOutput

SEARCH_CALLS = {"count": 0}
EMAIL_CALLS = {"count": 0}


@tool
def search_customer(customer_id: str) -> dict:
    """Search for a customer."""
    SEARCH_CALLS["count"] += 1
    return {
        "customer_id": customer_id,
        "allowed_to_contact": True,
        "summary": f"Customer {customer_id}",
    }


@tool
def send_email(customer_id: str, subject: str, body: str) -> dict:
    """Send email."""
    EMAIL_CALLS["count"] += 1
    return {"sent": True, "customer_id": customer_id}


@pytest.fixture
def policy_db():
    SEARCH_CALLS["count"] = 0
    EMAIL_CALLS["count"] = 0
    specs = tools_to_specs(
        [search_customer, send_email],
        output_schemas={
            "search_customer": CustomerOutput,
            "send_email": {"sent": bool, "customer_id": str},
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
    yield db
    db.close()


def test_missing_required_source_inserts_zero_rows(policy_db):
    runner = CallToolWithDataFlowRunner(policy_db)
    runner.run("INSERT INTO SearchCustomerInput (customer_id) VALUES ('c-1')")
    assert SEARCH_CALLS["count"] == 1

    payload = json.loads(
        runner.run(
            """
            INSERT INTO SendEmailInput (customer_id, subject, body)
            SELECT 'fabricated', 'hi', 'body'
            """
        )
    )
    assert payload["tool_calls_executed"] == 0
    assert EMAIL_CALLS["count"] == 0
    assert policy_db.fetchall("SELECT COUNT(*) FROM SendEmailInput")[0][0] == 0


def test_required_source_allows_insert_and_executes_tool(policy_db):
    runner = CallToolWithDataFlowRunner(policy_db)
    runner.run("INSERT INTO SearchCustomerInput (customer_id) VALUES ('c-9')")
    payload = json.loads(
        runner.run(
            """
            INSERT INTO SendEmailInput (customer_id, subject, body)
            SELECT
              SearchCustomerOutput.customer_id,
              'Follow up',
              SearchCustomerOutput.summary
            FROM SearchCustomerOutput
            WHERE SearchCustomerOutput.allowed_to_contact
            """
        )
    )
    assert payload["tool_calls_executed"] == 1
    assert EMAIL_CALLS["count"] == 1


def test_enforce_mode_hides_original_tools():
    from langchain_core.language_models.fake_chat_models import FakeListChatModel

    from data_flow_control import create_agent, langchain_dfc
    from data_flow_control.langchain.middleware import CALL_TOOL_WITH_DATAFLOW_NAME

    model = FakeListChatModel(responses=["ok"])
    tools = [search_customer, send_email]
    agent = create_agent(model=model, tools=tools)
    wrapped = langchain_dfc(
        agent,
        output_schemas={
            "search_customer": CustomerOutput,
            "send_email": {"sent": bool, "customer_id": str},
        },
        direct_tool_mode="enforce",
    )
    try:
        assert wrapped.visible_tool_names == {CALL_TOOL_WITH_DATAFLOW_NAME}
    finally:
        wrapped.close()
