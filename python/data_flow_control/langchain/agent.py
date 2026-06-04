"""LangChain agent configuration for Passant wrapping."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from langchain.agents import create_agent as _langchain_create_agent

from .middleware import CALL_TOOL_WITH_DATAFLOW_NAME
from .tool_spec import ToolSpec, tools_to_specs

PASSANT_CONFIG_ATTR = "_passant_langchain_config"


class UnsupportedAgentError(ValueError):
    """Raised when an agent cannot be wrapped through public LangChain hooks."""


@dataclass
class AgentConfig:
    model: Any
    tools: list[Any]
    middleware: list[Any] = field(default_factory=list)
    system_prompt: str | None = None
    create_agent_kwargs: dict[str, Any] = field(default_factory=dict)


def create_agent(
    model: Any,
    tools: list[Any],
    *,
    middleware: list[Any] | None = None,
    system_prompt: str | None = None,
    **create_agent_kwargs: Any,
) -> Any:
    """Create a LangChain agent and attach Passant wrap configuration."""
    agent = _langchain_create_agent(
        model=model,
        tools=tools,
        middleware=tuple(middleware or ()),
        system_prompt=system_prompt,
        **create_agent_kwargs,
    )
    return store_langchain_agent_config(
        agent,
        model=model,
        tools=tools,
        middleware=middleware,
        system_prompt=system_prompt,
        **create_agent_kwargs,
    )


def store_langchain_agent_config(
    agent: Any,
    *,
    model: Any,
    tools: list[Any],
    middleware: list[Any] | None = None,
    system_prompt: str | None = None,
    **create_agent_kwargs: Any,
) -> Any:
    """Attach Passant agent configuration using only public create_agent inputs."""
    config = AgentConfig(
        model=model,
        tools=list(tools),
        middleware=list(middleware or []),
        system_prompt=system_prompt,
        create_agent_kwargs=dict(create_agent_kwargs),
    )
    setattr(agent, PASSANT_CONFIG_ATTR, config)
    return agent


def resolve_agent_config(agent: Any) -> AgentConfig:
    stored = getattr(agent, PASSANT_CONFIG_ATTR, None)
    if stored is not None:
        return stored

    raise UnsupportedAgentError(
        "Could not resolve LangChain agent configuration for the provided agent. "
        "Create the agent with data_flow_control.langchain.create_agent(...), or call "
        "store_langchain_agent_config(agent, model=..., tools=...) after create_agent(...). "
        "Passant does not introspect private LangGraph state."
    )


def build_wrapped_agent(
    config: AgentConfig,
    *,
    visible_tools: list[Any],
    middleware: list[Any],
) -> Any:
    wrapped = _langchain_create_agent(
        model=config.model,
        tools=visible_tools,
        system_prompt=config.system_prompt,
        middleware=[*config.middleware, *middleware],
        **config.create_agent_kwargs,
    )
    setattr(wrapped, PASSANT_CONFIG_ATTR, config)
    return wrapped


def specs_from_agent_tools(
    tools: list[Any],
    *,
    output_schemas: dict[str, Any] | None,
) -> list[ToolSpec]:
    filtered = [
        tool for tool in tools if getattr(tool, "name", None) != CALL_TOOL_WITH_DATAFLOW_NAME
    ]
    return tools_to_specs(filtered, output_schemas=output_schemas)
