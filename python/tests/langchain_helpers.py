"""Shared helpers for LangChain integration tests."""

from __future__ import annotations

from typing import Any

from langchain_core.language_models.fake_chat_models import FakeMessagesListChatModel
from pydantic import BaseModel, Field

from data_flow_control import create_agent, langchain_dfc


class CustomerOutput(BaseModel):
    customer_id: str
    allowed_to_contact: bool = Field(default=True)
    summary: str = ""


class ScriptableToolChatModel(FakeMessagesListChatModel):
    """Fake chat model that supports create_agent tool binding in tests."""

    def bind_tools(self, tools, *, tool_choice=None, **kwargs):  # noqa: ANN001
        return self


def wrap_tools(
    *,
    model: Any,
    tools: list[Any],
    **langchain_dfc_kwargs: Any,
):
    agent = create_agent(model=model, tools=tools)
    return langchain_dfc(agent, **langchain_dfc_kwargs)


def make_configured_agent(model: Any, tools: list[Any], **create_agent_kwargs: Any):
    return create_agent(model=model, tools=tools, **create_agent_kwargs)
