from __future__ import annotations

from typing import Any


def build_catalog_snapshot(
    *,
    dialect: str,
    tables: dict[str, dict[str, Any]],
    default_schema: str | None = None,
    search_path: list[str] | None = None,
    unique_columns: list[list[str]] | None = None,
    aggregate_functions: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    """Build a normalized catalog snapshot for Rust validation."""
    snapshot: dict[str, Any] = {
        "dialect": dialect,
        "tables": {},
        "unique_columns": unique_columns or [],
    }
    if default_schema is not None:
        snapshot["default_schema"] = default_schema
    if search_path:
        snapshot["search_path"] = search_path
    for table_name, info in tables.items():
        column_types = info.get("types") or {}
        columns = info.get("columns") or list(column_types.keys())
        entry: dict[str, Any] = {
            "columns": columns,
            "types": column_types,
        }
        if "nullable" in info:
            entry["nullable"] = info["nullable"]
        if "row_count" in info:
            entry["row_count"] = info["row_count"]
        snapshot["tables"][table_name] = entry
    if aggregate_functions:
        snapshot["aggregate_functions"] = aggregate_functions
    return snapshot
