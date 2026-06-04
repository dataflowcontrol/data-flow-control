"""Tests for LangChain tool schema introspection."""

from __future__ import annotations

import pytest

pytestmark = pytest.mark.langchain

from langchain.tools import tool
from pydantic import BaseModel

from data_flow_control.langchain.schema import (
    input_table_name,
    output_table_name,
    python_type_to_duckdb,
    tool_name_to_table_base,
)
from data_flow_control.langchain.tool_spec import tool_to_spec, tools_to_specs
from langchain_helpers import CustomerOutput


def test_tool_name_to_table_base_snake_case():
    assert tool_name_to_table_base("search_customer") == "SearchCustomer"
    assert input_table_name("search_customer") == "SearchCustomerInput"
    assert output_table_name("search_customer") == "SearchCustomerOutput"


def test_tool_name_collision_rejected():
    @tool("Search")
    def search_a(value: str) -> str:
        """A."""
        return value

    @tool("search")
    def search_b(value: str) -> str:
        """B."""
        return value

    with pytest.raises(ValueError, match="collide after normalization"):
        tools_to_specs([search_a, search_b])


def test_input_schema_from_args_schema():
    class SearchArgs(BaseModel):
        customer_id: str
        include_history: bool = False

    @tool(args_schema=SearchArgs)
    def search_customer(customer_id: str, include_history: bool = False) -> dict:
        """Search customer."""
        return {"customer_id": customer_id}

    spec = tool_to_spec(search_customer)
    assert spec.input_schema == {
        "customer_id": "VARCHAR",
        "include_history": "BOOLEAN",
    }


def test_plain_callable_schema_extraction():
    def lookup(customer_id: str, limit: int) -> dict:
        """Look up a customer."""
        return {"customer_id": customer_id, "limit": limit}

    from langchain_core.tools import StructuredTool

    wrapped = StructuredTool.from_function(lookup)
    spec = tool_to_spec(wrapped)
    assert spec.input_schema == {"customer_id": "VARCHAR", "limit": "BIGINT"}


def test_single_string_tool_gets_input_column():
    @tool
    def echo(message: str) -> str:
        """Echo."""
        return message

    spec = tool_to_spec(echo)
    assert spec.input_schema == {"message": "VARCHAR"}


def test_output_schema_override():
    @tool
    def search_customer(customer_id: str) -> dict:
        """Search."""
        return {"customer_id": customer_id}

    spec = tool_to_spec(
        search_customer,
        output_schemas={"search_customer": CustomerOutput},
    )
    assert spec.output_schema == {
        "customer_id": "VARCHAR",
        "allowed_to_contact": "BOOLEAN",
        "summary": "VARCHAR",
    }


def test_python_type_mapping():
    assert python_type_to_duckdb(str) == "VARCHAR"
    assert python_type_to_duckdb(bool) == "BOOLEAN"
    assert python_type_to_duckdb(int) == "BIGINT"
    assert python_type_to_duckdb(float) == "DOUBLE"
    assert python_type_to_duckdb(dict) == "JSON"
    assert python_type_to_duckdb(str | None) == "VARCHAR"


def test_optional_and_union_string_mapping():
    from typing import Optional, Union

    assert python_type_to_duckdb(Optional[str]) == "VARCHAR"
    assert python_type_to_duckdb(Union[str, None]) == "VARCHAR"


def test_unsupported_agent_without_config():
    from langchain.agents import create_agent as langchain_create_agent
    from langchain_core.language_models.fake_chat_models import FakeListChatModel

    from data_flow_control import langchain_dfc
    from data_flow_control.langchain.agent import UnsupportedAgentError

    @tool
    def echo(value: str) -> str:
        """Echo."""
        return value

    agent = langchain_create_agent(model=FakeListChatModel(responses=["ok"]), tools=[echo])
    with pytest.raises(
        UnsupportedAgentError, match="Could not resolve LangChain agent configuration"
    ):
        langchain_dfc(agent)


def test_langchain_dfc_wraps_passant_create_agent():
    from langchain_core.language_models.fake_chat_models import FakeListChatModel

    from data_flow_control import create_agent, langchain_dfc

    @tool
    def echo(value: str) -> str:
        """Echo."""
        return value

    agent = create_agent(model=FakeListChatModel(responses=["ok"]), tools=[echo])
    wrapped = langchain_dfc(
        agent,
        output_schemas={"echo": {"value": str}},
    )
    try:
        assert "CallToolWithDataFlow" in wrapped.visible_tool_names
        assert "echo" in wrapped.visible_tool_names
    finally:
        wrapped.close()


def test_rejects_invalid_direct_tool_mode():
    from langchain_core.language_models.fake_chat_models import FakeListChatModel

    from data_flow_control import create_agent, langchain_dfc

    @tool
    def echo(value: str) -> str:
        """Echo."""
        return value

    agent = create_agent(model=FakeListChatModel(responses=["ok"]), tools=[echo])
    with pytest.raises(ValueError, match="direct_tool_mode must be 'observe' or 'enforce'"):
        langchain_dfc(agent, direct_tool_mode="enforc")


def test_async_only_tool_spec():
    from langchain_core.tools import StructuredTool

    async def lookup(value: str) -> dict:
        """Async lookup."""
        return {"value": value}

    tool = StructuredTool.from_function(coroutine=lookup)
    spec = tool_to_spec(tool, output_schemas={"lookup": {"value": str}})
    assert spec.input_schema == {"value": "VARCHAR"}
    assert spec.call_async is not None
    assert spec.call_sync is not None


def test_provider_tool_dict_rejected():
    with pytest.raises(ValueError, match="Provider built-in tool dictionaries"):
        tool_to_spec({"type": "function", "function": {"name": "x"}})
