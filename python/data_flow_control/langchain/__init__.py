"""LangChain integration for Passant data flow control."""

from __future__ import annotations

try:
    from .agent import create_agent, store_langchain_agent_config
    from .wrapper import LangChainDFC, langchain_dfc
except ModuleNotFoundError as exc:
    if exc.name in {"langchain", "langchain_core"}:
        raise ImportError(
            'LangChain integration requires: pip install "data-flow-control[langchain]"'
        ) from exc
    raise

__all__ = ["LangChainDFC", "create_agent", "langchain_dfc", "store_langchain_agent_config"]
