from __future__ import annotations

import sqlite3
from typing import Any

from ..catalog import build_catalog_snapshot
from .base import Capabilities
from .duckdb import quote_sql_identifier
from .kill import python_kill


class SQLiteAdapter:
    dialect = "sqlite"
    capabilities = Capabilities(exception_udf=True)

    def __init__(self, conn: sqlite3.Connection) -> None:
        self._conn = conn

    @property
    def connection(self) -> sqlite3.Connection:
        return self._conn

    def execute(self, sql: str, params: Any = None):
        if params is None:
            return self._conn.execute(sql)
        return self._conn.execute(sql, params)

    def quote_identifier(self, name: str) -> str:
        return quote_sql_identifier(name)

    def register_kill_function(self) -> None:
        self._conn.create_function("kill", 0, python_kill)

    def introspect_catalog(self) -> dict:
        tables: dict[str, dict] = {}
        rows = self.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'"
        ).fetchall()
        for (table_name,) in rows:
            info_rows = self.execute(
                f"PRAGMA table_info({self.quote_identifier(table_name)})"
            ).fetchall()
            column_types = {row[1]: str(row[2]).upper() for row in info_rows}
            tables[table_name] = {
                "columns": list(column_types.keys()),
                "types": column_types,
            }
        return build_catalog_snapshot(dialect=self.dialect, tables=tables, default_schema="main")

    def close(self) -> None:
        self._conn.close()
