from __future__ import annotations

import sqlite3
import sys
from dataclasses import replace
from typing import Any

from ..aggregate_introspection import introspect_sqlite_aggregates
from ..catalog import build_catalog_snapshot
from ._retry import register_retry
from .base import Capabilities
from .duckdb import quote_sql_identifier
from .kill import python_kill


class SQLiteAdapter:
    dialect = "sqlite"
    capabilities = Capabilities(exception_udf=True, retry_resolution=True)

    def __init__(self, conn: sqlite3.Connection) -> None:
        self._conn = conn
        self._retry_state = None
        self.retry_unavailable_reason = None
        if sqlite3.sqlite_version_info < (3, 44, 0):
            self.capabilities = replace(self.capabilities, retry_resolution=False)
            self.retry_unavailable_reason = (
                f"SQLite RETRY requires SQLite >= 3.44.0 (MATERIALIZED CTEs and concat_ws); "
                f"running SQLite {sqlite3.sqlite_version}"
            )

    @property
    def connection(self) -> sqlite3.Connection:
        return self._conn

    def execute(self, sql: str, params: Any = None):
        def run():
            cursor = self._conn.execute(sql) if params is None else self._conn.execute(sql, params)
            # SQLite steps lazily. Finish the single statement before exposing
            # any rows, while private callback evidence is still in scope.
            return _BufferedCursor(cursor, cursor.fetchall())

        if self._retry_state is None:
            return run()
        with self._retry_state.execution():
            return run()

    def register_retry_function(self) -> None:
        if self.retry_unavailable_reason is not None:
            raise ValueError(self.retry_unavailable_reason)
        self._retry_state = register_retry(
            self._conn,
            lambda state: self._conn.create_function("passant_retry_many", 1, state.signal),
        )

    def quote_identifier(self, name: str) -> str:
        return quote_sql_identifier(name)

    def register_kill_function(self) -> None:
        self._conn.create_function("kill", 0, python_kill)
        self._conn.create_function("passant_kill", 0, python_kill)

    def register_resolution_function(
        self,
        name: str,
        func: Any,
        parameter_types: list[Any],
        return_type: Any,
    ) -> None:
        raise ValueError(f"Tuple UDF resolution is not supported for dialect {self.dialect!r}")

    def register_relation_resolution_function(self, name: str, func: Any) -> None:
        raise ValueError(f"Relation UDF resolution is not supported for dialect {self.dialect!r}")

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
        return build_catalog_snapshot(
            dialect=self.dialect,
            tables=tables,
            default_schema="main",
            aggregate_functions=self.introspect_aggregate_functions(),
        )

    def introspect_aggregate_functions(self) -> list[dict]:
        return introspect_sqlite_aggregates(self._conn)

    def close(self) -> None:
        self._conn.close()


class _BufferedCursor:
    def __init__(self, cursor, rows):
        self._cursor = cursor
        self._rows = iter(rows)
        self.arraysize = cursor.arraysize

    def fetchone(self):
        # The native cursor is exhausted, but still validates cursor/connection
        # lifetime. This never re-executes the statement or steps unread rows.
        self._cursor.fetchone()
        return next(self._rows, None)

    def fetchall(self):
        self._cursor.fetchall()
        return list(self._rows)

    def fetchmany(self, *args, **kwargs):
        from itertools import islice
        from operator import index

        self._cursor.arraysize = self.arraysize
        self._cursor.fetchmany(*args, **kwargs)
        size = index(args[0] if args else kwargs.get("size", self.arraysize))
        # Python 3.14 changed zero from "all remaining" to an empty batch.
        # Negative-size validation is delegated to the native cursor above.
        if size == 0 and sys.version_info >= (3, 14):
            return []
        return list(self._rows) if size <= 0 else list(islice(self._rows, size))

    def __iter__(self):
        return self

    def __next__(self):
        self._cursor.fetchone()
        return next(self._rows)

    def close(self):
        self._cursor.close()

    def __getattr__(self, name):
        return getattr(self._cursor, name)
