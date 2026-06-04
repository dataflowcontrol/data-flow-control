"""SQL validation for CallToolWithDataFlow."""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Literal

import sqlglot
from sqlglot import exp

from .schema import PASSANT_PREFIX

StatementType = Literal["select", "insert", "other"]


@dataclass(frozen=True)
class ValidatedSql:
    sql: str
    statement_type: StatementType


_DDL = re.compile(r"^\s*(CREATE|DROP|ALTER)\s+", re.IGNORECASE)
_DESTRUCTIVE = re.compile(r"^\s*(DELETE|UPDATE|TRUNCATE)\s+", re.IGNORECASE)


def validate_call_tool_sql(
    sql: str,
    *,
    allowed_input_tables: set[str],
    protected_tables: set[str],
) -> ValidatedSql:
    stripped = sql.strip()
    if not stripped:
        raise ValueError("SQL must not be empty")
    if ";" in stripped.rstrip(";"):
        raise ValueError("Only a single SQL statement is allowed")
    if _DDL.match(stripped):
        raise ValueError("DDL statements are not allowed")
    if _DESTRUCTIVE.match(stripped):
        raise ValueError("Destructive DML statements are not allowed")

    try:
        parsed = sqlglot.parse_one(stripped, read="duckdb")
    except Exception as exc:
        raise ValueError(f"Invalid SQL: {exc}") from exc

    if isinstance(parsed, exp.Insert):
        sink = _insert_target_table(parsed)
        if sink is None:
            raise ValueError("Could not determine INSERT target table")
        sink_lower = sink.lower()
        if sink_lower in {table.lower() for table in protected_tables}:
            raise ValueError(f"Writes to protected table {sink!r} are not allowed")
        allowed_lower = {table.lower() for table in allowed_input_tables}
        if sink_lower not in allowed_lower:
            raise ValueError(
                f"INSERT target {sink!r} is not an allowed tool input table; "
                f"allowed targets: {sorted(allowed_input_tables)}"
            )
        _validate_insert_columns(parsed)
        return ValidatedSql(sql=stripped, statement_type="insert")

    if isinstance(parsed, (exp.Select, exp.Union, exp.Intersect, exp.Except)):
        return ValidatedSql(sql=stripped, statement_type="select")

    raise ValueError("Only SELECT and INSERT statements are allowed")


def validate_debug_fetch_sql(sql: str) -> str:
    """Allow SELECT-only SQL for LangChainDFC.fetchall debugging helpers."""
    stripped = sql.strip()
    if not stripped:
        raise ValueError("SQL must not be empty")
    if ";" in stripped.rstrip(";"):
        raise ValueError("Only a single SQL statement is allowed")
    if _DDL.match(stripped):
        raise ValueError("fetchall only supports SELECT statements")
    if _DESTRUCTIVE.match(stripped):
        raise ValueError("fetchall only supports SELECT statements")

    try:
        parsed = sqlglot.parse_one(stripped, read="duckdb")
    except Exception as exc:
        raise ValueError(f"Invalid SQL: {exc}") from exc

    if isinstance(parsed, (exp.Select, exp.Union, exp.Intersect, exp.Except)):
        return stripped

    raise ValueError("fetchall only supports SELECT statements")


def _insert_target_table(parsed: exp.Insert) -> str | None:
    target = parsed.this
    if isinstance(target, exp.Schema):
        target = target.this
    if isinstance(target, exp.Table):
        return target.name
    return None


def _validate_insert_columns(parsed: exp.Insert) -> None:
    schema = parsed.this if isinstance(parsed.this, exp.Schema) else None
    if schema is None or not schema.expressions:
        raise ValueError(
            "INSERT must specify an explicit non-metadata column list, for example "
            "INSERT INTO ToolInput (field_a, field_b) VALUES (...)"
        )

    for column in schema.expressions:
        name = _column_name(column)
        if name is None:
            raise ValueError("INSERT column list must use simple column names")
        if name.startswith(PASSANT_PREFIX):
            raise ValueError(f"INSERT must not target reserved Passant metadata column {name!r}")


def _column_name(column: exp.Expression) -> str | None:
    if isinstance(column, exp.Column):
        return column.name
    if isinstance(column, exp.Identifier):
        return column.this
    return None
