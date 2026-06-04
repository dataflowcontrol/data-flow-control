"""Helpers to introspect aggregate functions from database connections."""

from __future__ import annotations

from typing import Any


def normalize_aggregate_entry(
    name: str,
    *,
    schema: str | None = None,
    aliases: list[str] | None = None,
    classification: str | None = None,
    source: str = "introspected",
) -> dict[str, Any]:
    entry: dict[str, Any] = {"name": name, "source": source}
    if schema is not None:
        entry["schema"] = schema
    if aliases:
        entry["aliases"] = aliases
    if classification is not None:
        entry["classification"] = classification
    return entry


def introspect_duckdb_aggregates(conn: Any) -> list[dict[str, Any]]:
    rows = conn.execute(
        """
        SELECT function_name
        FROM duckdb_functions()
        WHERE function_type = 'aggregate'
        """
    ).fetchall()
    return [normalize_aggregate_entry(row[0]) for row in rows if row and row[0]]


SQLITE_BUILTIN_AGGREGATE_ALLOWLIST = frozenset(
    {
        "avg",
        "count",
        "group_concat",
        "max",
        "min",
        "sum",
        "total",
        "median",
        "percentile",
        "percentile_cont",
        "percentile_disc",
        "string_agg",
        "json_group_array",
        "json_group_object",
    }
)


def introspect_sqlite_aggregates(conn: Any) -> list[dict[str, Any]]:
    rows = conn.execute("PRAGMA function_list").fetchall()
    aggregates: list[dict[str, Any]] = []
    seen: set[str] = set()
    for row in rows:
        if len(row) < 3:
            continue
        name = str(row[0])
        func_type = str(row[2]).lower()
        key = name.lower()
        if key in seen:
            continue
        if func_type == "a":
            seen.add(key)
            aggregates.append(normalize_aggregate_entry(name, classification="unknown_custom"))
        elif func_type == "w" and key in SQLITE_BUILTIN_AGGREGATE_ALLOWLIST:
            seen.add(key)
            aggregates.append(normalize_aggregate_entry(name))
    return aggregates


def introspect_clickhouse_aggregates(conn: Any) -> list[dict[str, Any]]:
    rows = conn.execute(
        "SELECT name, alias_to FROM system.functions WHERE is_aggregate = 1"
    ).fetchall()
    aggregates: list[dict[str, Any]] = []
    seen: set[str] = set()
    for row in rows:
        name = str(row[0])
        key = name.lower()
        if key not in seen:
            seen.add(key)
            aggregates.append(normalize_aggregate_entry(name))
        alias_to = row[1] if len(row) > 1 else None
        if alias_to:
            alias = str(alias_to)
            alias_key = alias.lower()
            if alias_key not in seen:
                seen.add(alias_key)
                aggregates.append(normalize_aggregate_entry(alias, aliases=[name]))
    return aggregates


def introspect_datafusion_aggregates(ctx: Any) -> list[dict[str, Any]]:
    try:
        rows = ctx.sql("SHOW FUNCTIONS").collect()
    except Exception:
        return []
    aggregates: list[dict[str, Any]] = []
    for row in rows:
        values = list(row)
        if not values:
            continue
        name = str(values[0])
        func_type = str(values[1]).upper() if len(values) > 1 else ""
        if func_type in ("AGGREGATE", "UDAF") or "aggregate" in func_type.lower():
            aggregates.append(normalize_aggregate_entry(name))
    return aggregates
