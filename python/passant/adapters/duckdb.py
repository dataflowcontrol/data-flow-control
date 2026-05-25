from __future__ import annotations

from typing import Any

import duckdb

from .base import Capabilities


def quote_sql_identifier(name: str) -> str:
    """Quote a DuckDB table identifier, including schema-qualified names."""
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
    capabilities = Capabilities(supports_kill=True)

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
        def _kill() -> bool:
            raise ValueError("KILLing due to dfc policy violation")

        for name in ("kill", "passant_kill"):
            try:
                self._conn.create_function(name, _kill, [], "BOOLEAN")
            except duckdb.Error:
                pass

    def introspect_catalog(self) -> dict:
        tables: dict[str, dict] = {}
        for table_name in self._list_catalog_tables():
            column_types = self._get_table_columns(table_name)
            tables[table_name] = {
                "columns": list(column_types.keys()),
                "types": column_types,
            }
        return {"tables": tables, "unique_columns": []}

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

    def _get_table_columns(self, table_name: str) -> dict[str, str]:
        try:
            rows = self.execute(f"DESCRIBE {self.quote_identifier(table_name)}").fetchall()
        except duckdb.Error as exc:
            raise ValueError(f"Table '{table_name}' does not exist") from exc
        return {row[0]: str(row[1]).upper() for row in rows}

    def close(self) -> None:
        self._conn.close()
