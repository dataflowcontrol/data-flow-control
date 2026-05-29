from __future__ import annotations

from typing import Any

import duckdb

from ..catalog import build_catalog_snapshot
from .base import Capabilities
from .kill import python_kill


def quote_sql_identifier(name: str) -> str:
    """Quote a SQL identifier, including schema-qualified names."""
    name = name.strip()
    if not name:
        raise ValueError("Table name must be non-empty")

    def quote_part(part: str) -> str:
        part = part.strip()
        if part.startswith('"') and part.endswith('"'):
            return part
        escaped = part.replace('"', '""')
        return f'"{escaped}"'

    return ".".join(quote_part(part) for part in name.split("."))


class DuckDBAdapter:
    dialect = "duckdb"
    capabilities = Capabilities(
        exception_udf=True,
        tuple_udf=True,
        relation_udf=True,
    )

    def __init__(self, conn: duckdb.DuckDBPyConnection) -> None:
        self._conn = conn

    @property
    def connection(self) -> duckdb.DuckDBPyConnection:
        return self._conn

    def execute(self, sql: str, params: Any = None):
        if params is None:
            return self._conn.execute(sql)
        return self._conn.execute(sql, params)

    def quote_identifier(self, name: str) -> str:
        return quote_sql_identifier(name)

    def register_kill_function(self) -> None:
        for name in ("kill", "passant_kill"):
            try:
                self._conn.create_function(name, python_kill, [], "BOOLEAN")
            except duckdb.Error:
                pass

    def register_resolution_function(
        self,
        name: str,
        func: Any,
        parameter_types: list[Any],
        return_type: Any,
    ) -> None:
        self._conn.create_function(
            name,
            func,
            parameter_types,
            return_type,
            null_handling="special",
        )

    def register_relation_resolution_function(self, name: str, func: Any) -> None:
        """Register a scalar UDF invoked with bool_or(violation) over the relation input."""
        self._conn.create_function(name, func, ["BOOLEAN"], "BOOLEAN")

    def introspect_catalog(self) -> dict:
        tables: dict[str, dict] = {}
        unique_columns: list[list[str]] = []
        for table_name in self._list_catalog_tables():
            column_types = self._get_table_columns(table_name)
            if column_types is None:
                # Extension-internal tables (e.g. Flock) may appear in duckdb_tables()
                # but are not describable with DESCRIBE; skip them for policy catalog sync.
                continue
            row_count = self._table_row_count(table_name)
            entry: dict = {
                "columns": list(column_types.keys()),
                "types": column_types,
            }
            if row_count is not None:
                entry["row_count"] = row_count
            tables[table_name] = entry
            unique_columns.extend(self._primary_key_columns(table_name))
        return build_catalog_snapshot(
            dialect=self.dialect,
            tables=tables,
            default_schema="main",
            search_path=["main"],
            unique_columns=unique_columns,
        )

    def _list_catalog_tables(self) -> list[str]:
        try:
            rows = self.execute(
                "SELECT schema_name, table_name FROM duckdb_tables() "
                "WHERE NOT internal AND NOT temporary"
            ).fetchall()
        except duckdb.Error:
            rows = [("main", name) for (name,) in self.execute("SHOW TABLES").fetchall()]
        tables: list[str] = []
        for schema, name in rows:
            if schema and schema.lower() not in ("main", "temp"):
                tables.append(f"{schema}.{name}")
            else:
                tables.append(name)
        return tables

    def _primary_key_columns(self, table_name: str) -> list[list[str]]:
        try:
            rows = self.execute(
                "SELECT constraint_column_names FROM duckdb_constraints() "
                "WHERE constraint_type = 'PRIMARY KEY' AND table_name = ?",
                [table_name.split(".")[-1]],
            ).fetchall()
        except duckdb.Error:
            return []
        keys: list[list[str]] = []
        for (column_names,) in rows:
            for column in column_names:
                keys.append([table_name, column])
        return keys

    def _table_row_count(self, table_name: str) -> int | None:
        try:
            row = self.execute(
                f"SELECT COUNT(*) FROM {self.quote_identifier(table_name)}"
            ).fetchone()
        except duckdb.Error:
            return None
        if row is None:
            return None
        return int(row[0])

    def _get_table_columns(self, table_name: str) -> dict[str, str] | None:
        try:
            rows = self.execute(f"DESCRIBE {self.quote_identifier(table_name)}").fetchall()
        except duckdb.Error:
            return None
        return {row[0]: str(row[1]).upper() for row in rows}

    def close(self) -> None:
        self._conn.close()
