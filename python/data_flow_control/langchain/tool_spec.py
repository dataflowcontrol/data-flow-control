"""LangChain tool introspection for Passant tool-use tables."""

from __future__ import annotations

import asyncio
import inspect
from collections.abc import Awaitable, Callable, Mapping
from dataclasses import dataclass
from typing import Any

from .schema import normalize_input_schema, normalize_output_schema


@dataclass(frozen=True)
class ToolSpec:
    name: str
    description: str
    input_schema: dict[str, str]
    output_schema: dict[str, str]
    call_sync: Callable[..., Any]
    call_async: Callable[..., Awaitable[Any]] | None
    original_tool: Any


def _is_provider_tool_dict(tool: Any) -> bool:
    return isinstance(tool, dict) and {"type", "function"}.issubset(tool.keys())


def _callable_input_schema(func: Callable[..., Any]) -> dict[str, str]:
    signature = inspect.signature(func)
    params = [
        param
        for param in signature.parameters.values()
        if param.kind
        in (
            inspect.Parameter.POSITIONAL_OR_KEYWORD,
            inspect.Parameter.KEYWORD_ONLY,
        )
    ]
    if len(params) == 1 and params[0].annotation in (str, inspect._empty):
        return {"input": "VARCHAR"}
    schema: dict[str, str] = {}
    for param in params:
        schema[param.name] = normalize_input_schema({param.name: param.annotation})[param.name]
    return schema or {"input": "VARCHAR"}


def _callable_output_schema(func: Callable[..., Any]) -> dict[str, str]:
    return_annotation = inspect.signature(func).return_annotation
    if return_annotation is inspect._empty:
        return {}
    try:
        return normalize_output_schema(return_annotation)
    except TypeError:
        return {}


def _make_async_caller(func: Callable[..., Any]) -> Callable[..., Awaitable[Any]] | None:
    if inspect.iscoroutinefunction(func):
        return func

    async def _async_wrapper(*args: Any, **kwargs: Any) -> Any:
        return await asyncio.to_thread(func, *args, **kwargs)

    return _async_wrapper


def _extract_from_base_tool(
    tool: Any,
    *,
    output_schemas: Mapping[str, Any] | None,
) -> ToolSpec:
    from langchain_core.tools import BaseTool

    if not isinstance(tool, BaseTool):
        raise TypeError(f"Expected BaseTool, got {type(tool)!r}")

    name = tool.name
    description = tool.description or ""

    sync_func = tool.func
    async_func = getattr(tool, "coroutine", None)
    if sync_func is None and async_func is None:
        raise ValueError(f"Tool {name!r} does not expose a callable func or coroutine")

    if tool.args_schema is not None:
        input_schema = normalize_input_schema(tool.args_schema)
    elif getattr(tool, "args", None):
        input_schema = normalize_input_schema({arg: str for arg in tool.args})
    elif sync_func is not None:
        input_schema = _callable_input_schema(sync_func)
    elif async_func is not None:
        input_schema = _callable_input_schema(async_func)
    else:
        input_schema = {"input": "VARCHAR"}

    output_schema: dict[str, str] = {}
    if output_schemas and name in output_schemas:
        output_schema = normalize_output_schema(output_schemas[name])
    elif tool.metadata and tool.metadata.get("passant_output_schema") is not None:
        output_schema = normalize_output_schema(tool.metadata["passant_output_schema"])
    elif sync_func is not None:
        output_schema = _callable_output_schema(sync_func)
    elif async_func is not None:
        output_schema = _callable_output_schema(async_func)

    if async_func is None and sync_func is not None:
        async_func = _make_async_caller(sync_func)

    if sync_func is None and async_func is not None:

        def _sync_from_async(**kwargs: Any) -> Any:
            return asyncio.run(async_func(**kwargs))

        sync_func = _sync_from_async

    return ToolSpec(
        name=name,
        description=description,
        input_schema=input_schema,
        output_schema=output_schema,
        call_sync=sync_func,
        call_async=async_func,
        original_tool=tool,
    )


def tool_to_spec(
    tool: Any,
    *,
    output_schemas: Mapping[str, Any] | None = None,
) -> ToolSpec:
    if _is_provider_tool_dict(tool):
        raise ValueError(
            "Provider built-in tool dictionaries are not supported; pass executable tools"
        )

    from langchain_core.tools import BaseTool

    if isinstance(tool, BaseTool):
        return _extract_from_base_tool(tool, output_schemas=output_schemas)

    if callable(tool):
        from langchain_core.tools import StructuredTool

        if hasattr(tool, "name") and hasattr(tool, "invoke"):
            return _extract_from_base_tool(tool, output_schemas=output_schemas)

        wrapped = StructuredTool.from_function(tool)
        return _extract_from_base_tool(wrapped, output_schemas=output_schemas)

    raise TypeError(f"Unsupported tool type: {type(tool)!r}")


def tools_to_specs(
    tools: list[Any],
    *,
    output_schemas: Mapping[str, Any] | None = None,
) -> list[ToolSpec]:
    from .schema import normalize_tool_names

    specs = [tool_to_spec(tool, output_schemas=output_schemas) for tool in tools]
    normalize_tool_names([spec.name for spec in specs])
    return specs
