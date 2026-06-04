"""Schema helpers for LangChain tool table DDL."""

from __future__ import annotations

import datetime as dt
import json
import re
import types
from typing import Any, Union, get_args, get_origin

PASSANT_PREFIX = "__passant_"
RESERVED_METADATA_COLUMNS = frozenset(
    {
        "__passant_input_id",
        "__passant_output_id",
        "__passant_tool_name",
        "__passant_tool_call_id",
        "__passant_origin",
        "__passant_sql_call_id",
        "__passant_thread_id",
        "__passant_run_id",
        "__passant_created_at",
        "__passant_executed_at",
        "__passant_status",
        "__passant_error",
        "__passant_raw_json",
    }
)

_SQL_IDENTIFIER = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")


def tool_name_to_table_base(tool_name: str) -> str:
    """Map a LangChain tool name to a SQL-friendly PascalCase table base name."""
    if not tool_name.strip():
        raise ValueError("Tool name must not be empty")
    if PASSANT_PREFIX in tool_name:
        raise ValueError(f"Tool name must not contain reserved prefix {PASSANT_PREFIX!r}")
    parts = re.split(r"[_\-\s]+", tool_name.strip())
    pascal = "".join(part[:1].upper() + part[1:] for part in parts if part)
    if not pascal:
        raise ValueError(f"Tool name {tool_name!r} does not produce a valid table base name")
    if not _SQL_IDENTIFIER.match(pascal):
        raise ValueError(
            f"Tool name {tool_name!r} maps to invalid SQL identifier {pascal!r}; "
            "use a simpler tool name"
        )
    return pascal


def input_table_name(tool_name: str) -> str:
    return f"{tool_name_to_table_base(tool_name)}Input"


def output_table_name(tool_name: str) -> str:
    return f"{tool_name_to_table_base(tool_name)}Output"


def quote_identifier(name: str) -> str:
    return f'"{name.replace(chr(34), chr(34) * 2)}"'


def normalize_tool_names(tool_names: list[str]) -> dict[str, str]:
    """Return tool_name -> table_base and reject normalized collisions."""
    mapping: dict[str, str] = {}
    normalized_to_tool: dict[str, str] = {}
    for name in tool_names:
        base = tool_name_to_table_base(name)
        key = base.lower()
        if key in normalized_to_tool and normalized_to_tool[key] != name:
            raise ValueError(
                f"Tool names {normalized_to_tool[key]!r} and {name!r} collide after normalization"
            )
        normalized_to_tool[key] = name
        mapping[name] = base
    return mapping


def python_type_to_duckdb(type_hint: Any) -> str:
    if type_hint is Any or type_hint is None:
        return "JSON"

    if isinstance(type_hint, types.UnionType):
        non_none = [arg for arg in type_hint.__args__ if arg is not type(None)]
        if len(non_none) == 1:
            return python_type_to_duckdb(non_none[0])
        return "JSON"

    origin = get_origin(type_hint)
    if origin is Union or str(origin) == "typing.Union":
        args = get_args(type_hint)
        non_none = [arg for arg in args if arg is not type(None)]
        if len(non_none) == 1:
            return python_type_to_duckdb(non_none[0])
        return "JSON"

    if origin is not None:
        if origin is list or origin is dict or origin is tuple or origin is set:
            return "JSON"
    if type_hint is str:
        return "VARCHAR"
    if type_hint is bool:
        return "BOOLEAN"
    if type_hint is int:
        return "BIGINT"
    if type_hint is float:
        return "DOUBLE"
    if type_hint in (dt.datetime,):
        return "TIMESTAMP"
    if type_hint in (dt.date,):
        return "DATE"
    if isinstance(type_hint, type):
        if issubclass(type_hint, str):
            return "VARCHAR"
        if issubclass(type_hint, bool):
            return "BOOLEAN"
        if issubclass(type_hint, int):
            return "BIGINT"
        if issubclass(type_hint, float):
            return "DOUBLE"
        if issubclass(type_hint, (dict, list, tuple, set)):
            return "JSON"
    return "JSON"


def fields_from_mapping(schema: dict[str, Any]) -> dict[str, str]:
    columns: dict[str, str] = {}
    for name, type_hint in schema.items():
        if name.startswith(PASSANT_PREFIX):
            raise ValueError(f"Field name {name!r} collides with reserved Passant prefix")
        columns[name] = python_type_to_duckdb(type_hint)
    return columns


def fields_from_pydantic(model_type: Any) -> dict[str, str]:
    if hasattr(model_type, "model_fields"):
        fields = model_type.model_fields
    elif hasattr(model_type, "__fields__"):
        fields = model_type.__fields__
    else:
        raise TypeError(f"Unsupported Pydantic model type: {model_type!r}")
    columns: dict[str, str] = {}
    for name, field in fields.items():
        if name.startswith(PASSANT_PREFIX):
            raise ValueError(f"Field name {name!r} collides with reserved Passant prefix")
        annotation = getattr(field, "annotation", Any)
        columns[name] = python_type_to_duckdb(annotation)
    return columns


def normalize_output_schema(schema: Any) -> dict[str, str]:
    if schema is None:
        return {}
    if isinstance(schema, dict):
        return fields_from_mapping(schema)
    if isinstance(schema, type):
        return fields_from_pydantic(schema)
    raise TypeError(f"Unsupported output schema type: {type(schema)!r}")


def normalize_input_schema(schema: Any) -> dict[str, str]:
    if schema is None:
        return {"input": "VARCHAR"}
    if isinstance(schema, dict):
        if not schema:
            return {"input": "VARCHAR"}
        return fields_from_mapping(schema)
    if isinstance(schema, type):
        return fields_from_pydantic(schema)
    raise TypeError(f"Unsupported input schema type: {type(schema)!r}")


def schema_to_json(schema: dict[str, str]) -> str:
    return json.dumps(schema, sort_keys=True)
