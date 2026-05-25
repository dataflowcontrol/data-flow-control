from __future__ import annotations

from typing import Any

from ..catalog import build_catalog_snapshot


def introspect_pg_catalog(conn: Any, *, dialect: str) -> dict:
    if dialect == "umbra":
        return _introspect_umbra_catalog(conn)
    return _introspect_information_schema(conn, dialect=dialect)


def _introspect_information_schema(conn: Any, *, dialect: str) -> dict:
    cursor = conn.cursor()
    cursor.execute(
        """
        SELECT table_schema, table_name, column_name, data_type
        FROM information_schema.columns
        WHERE table_schema NOT IN ('pg_catalog', 'information_schema')
        ORDER BY table_schema, table_name, ordinal_position
        """
    )
    return _snapshot_from_rows(cursor.fetchall(), dialect=dialect)


def _introspect_umbra_catalog(conn: Any) -> dict:
    cursor = conn.cursor()
    cursor.execute(
        """
        SELECT n.nspname, c.relname, a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod)
        FROM pg_catalog.pg_class c
        JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
        JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid
        WHERE c.relkind = 'r'
          AND NOT a.attisdropped
          AND n.nspname NOT IN ('pg_catalog', 'pg_temp', 'umbra')
        ORDER BY n.nspname, c.relname, a.attnum
        """
    )
    return _snapshot_from_rows(cursor.fetchall(), dialect="umbra")


def _snapshot_from_rows(rows: list[tuple], *, dialect: str) -> dict:
    tables: dict[str, dict] = {}
    for schema, table, column, data_type in rows:
        key = f"{schema}.{table}" if schema and schema != "public" else table
        entry = tables.setdefault(key, {"columns": [], "types": {}})
        entry["columns"].append(column)
        entry["types"][column] = str(data_type).upper()
    return build_catalog_snapshot(
        dialect=dialect,
        tables=tables,
        default_schema="public",
        search_path=["public"],
    )
