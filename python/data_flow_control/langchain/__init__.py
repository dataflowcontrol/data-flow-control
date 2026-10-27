"""Optional LangChain adapter; tool-table primitives do not require LangChain."""

from __future__ import annotations

__all__ = ["LangChainDFC", "create_agent", "langchain_dfc", "store_langchain_agent_config"]


def __getattr__(name: str):
    try:
        if name in {"create_agent", "store_langchain_agent_config"}:
            from . import agent

            return getattr(agent, name)
        if name in {"LangChainDFC", "langchain_dfc"}:
            from . import wrapper

            return getattr(wrapper, name)
    except ModuleNotFoundError as exc:
        if exc.name in {"langchain", "langchain_core"}:
            raise ImportError(
                'LangChain integration requires: pip install "data-flow-control[langchain]"'
            ) from exc
        raise
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
