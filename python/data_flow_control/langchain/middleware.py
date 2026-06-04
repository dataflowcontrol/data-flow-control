"""LangChain middleware for logging direct tool calls."""

from __future__ import annotations

from typing import Any

from langchain.agents.middleware import AgentMiddleware

from .database import ToolUseDatabase

CALL_TOOL_WITH_DATAFLOW_NAME = "CallToolWithDataFlow"


def _runtime_metadata(request: Any) -> dict[str, str | None]:
    tool_call = getattr(request, "tool_call", {}) or {}
    runtime = getattr(request, "runtime", None)
    config = getattr(runtime, "config", {}) if runtime is not None else {}
    configurable = config.get("configurable", {}) if isinstance(config, dict) else {}
    return {
        "tool_call_id": tool_call.get("id"),
        "thread_id": configurable.get("thread_id"),
        "run_id": config.get("run_id") if isinstance(config, dict) else None,
    }


def _should_log_tool(db: ToolUseDatabase, tool_name: str | None) -> bool:
    if not tool_name or tool_name == CALL_TOOL_WITH_DATAFLOW_NAME:
        return False
    return tool_name in db.spec_by_name


def _log_direct_tool_call(
    db: ToolUseDatabase,
    request: Any,
    result: Any,
    *,
    error: BaseException | None = None,
) -> None:
    tool_name = request.tool_call.get("name")
    if not _should_log_tool(db, tool_name):
        return

    metadata = _runtime_metadata(request)
    args = dict(request.tool_call.get("args") or {})
    input_id = db.insert_direct_input_row(
        tool_name,
        args,
        tool_call_id=metadata["tool_call_id"],
        thread_id=metadata["thread_id"],
        run_id=metadata["run_id"],
    )
    if error is None:
        db.insert_output_row(
            tool_name,
            input_id=input_id,
            status="succeeded",
            payload=_tool_message_content(result),
            tool_call_id=metadata["tool_call_id"],
            thread_id=metadata["thread_id"],
            run_id=metadata["run_id"],
        )
        db.mark_input_status(tool_name, input_id, status="succeeded")
        return

    db.insert_output_row(
        tool_name,
        input_id=input_id,
        status="failed",
        payload=None,
        error=str(error),
        tool_call_id=metadata["tool_call_id"],
        thread_id=metadata["thread_id"],
        run_id=metadata["run_id"],
    )
    db.mark_input_status(tool_name, input_id, status="failed", error=str(error))


class PassantToolLoggingMiddleware(AgentMiddleware):
    """Log direct LangChain tool calls into Passant input/output tables."""

    name = "passant_tool_logging"

    def __init__(self, db: ToolUseDatabase) -> None:
        self.db = db

    def wrap_tool_call(self, request, handler):
        tool_name = request.tool_call.get("name")
        if not _should_log_tool(self.db, tool_name):
            return handler(request)

        try:
            result = handler(request)
        except Exception as exc:
            _log_direct_tool_call(self.db, request, None, error=exc)
            raise
        _log_direct_tool_call(self.db, request, result)
        return result

    async def awrap_tool_call(self, request, handler):
        tool_name = request.tool_call.get("name")
        if not _should_log_tool(self.db, tool_name):
            return await handler(request)

        try:
            result = await handler(request)
        except Exception as exc:
            _log_direct_tool_call(self.db, request, None, error=exc)
            raise
        _log_direct_tool_call(self.db, request, result)
        return result


def build_passant_tool_middleware(db: ToolUseDatabase) -> PassantToolLoggingMiddleware:
    """Return middleware with both sync and async tool-call hooks."""
    return PassantToolLoggingMiddleware(db)


def _tool_message_content(result: Any) -> Any:
    content = getattr(result, "content", result)
    if isinstance(content, str):
        try:
            import json

            return json.loads(content)
        except Exception:
            return content
    return content
