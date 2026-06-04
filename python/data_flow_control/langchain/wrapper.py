"""Public LangChain DFC wrapper."""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any, Literal

from data_flow_control.policy import Policy

from .agent import (
    AgentConfig,
    build_wrapped_agent,
    resolve_agent_config,
    specs_from_agent_tools,
)
from .call_tool_with_dataflow import build_call_tool_with_dataflow
from .database import ToolUseDatabase, parse_direct_tool_mode
from .middleware import build_passant_tool_middleware

DirectToolModeArg = Literal["observe", "enforce"]


class LangChainDFC:
    """Controller for a Passant-wrapped LangChain agent."""

    def __init__(
        self,
        *,
        agent: Any,
        db: ToolUseDatabase,
        config: AgentConfig,
        passant_middleware: Any,
        dfc_tool: Any,
        visible_tool_names: set[str],
    ) -> None:
        self.agent = agent
        self.db = db
        self._config = config
        self._passant_middleware = passant_middleware
        self._dfc_tool = dfc_tool
        self.visible_tool_names = visible_tool_names

    def invoke(self, input: Any, /, **kwargs: Any) -> Any:
        return self.agent.invoke(input, **kwargs)

    async def ainvoke(self, input: Any, /, **kwargs: Any) -> Any:
        return await self.agent.ainvoke(input, **kwargs)

    def stream(self, input: Any, /, **kwargs: Any):
        return self.agent.stream(input, **kwargs)

    async def astream(self, input: Any, /, **kwargs: Any):
        async for item in self.agent.astream(input, **kwargs):
            yield item

    def register_policy(self, policy: Policy) -> None:
        self.db.register_policy(policy)

    def register_policies(self, policies: list[Policy]) -> None:
        self.db.register_policies(policies)

    def fetchall(self, sql: str) -> list[tuple]:
        return self.db.fetchall(sql)

    def close(self) -> None:
        self.db.close()

    def __enter__(self) -> LangChainDFC:
        return self

    def __exit__(self, exc_type, exc_val, exc_tb) -> None:
        self.close()


def langchain_dfc(
    agent: Any,
    *,
    policies: list[Policy] | None = None,
    db_path: str | None = None,
    output_schemas: Mapping[str, Any] | None = None,
    direct_tool_mode: DirectToolModeArg = "observe",
) -> LangChainDFC:
    """Wrap a LangChain 1.x agent with Passant data-flow control over tool I/O tables.

    Pass the agent created with ``data_flow_control.langchain.create_agent(...)`` or
    configured with ``store_langchain_agent_config(...)``.
    """
    mode = parse_direct_tool_mode(direct_tool_mode)
    config = resolve_agent_config(agent)
    specs = specs_from_agent_tools(config.tools, output_schemas=output_schemas)
    db = ToolUseDatabase(
        specs,
        db_path=db_path,
        direct_tool_mode=mode,
        policies=policies,
    )
    passant_middleware = build_passant_tool_middleware(db)
    dfc_tool = build_call_tool_with_dataflow(db)

    if mode == "enforce":
        visible_tools = [dfc_tool]
    else:
        visible_tools = [*config.tools, dfc_tool]

    visible_tool_names = {
        name for tool in visible_tools for name in [getattr(tool, "name", None)] if name
    }

    wrapped_agent = build_wrapped_agent(
        config,
        visible_tools=visible_tools,
        middleware=[passant_middleware],
    )

    return LangChainDFC(
        agent=wrapped_agent,
        db=db,
        config=config,
        passant_middleware=passant_middleware,
        dfc_tool=dfc_tool,
        visible_tool_names=visible_tool_names,
    )


__all__ = [
    "LangChainDFC",
    "langchain_dfc",
]
