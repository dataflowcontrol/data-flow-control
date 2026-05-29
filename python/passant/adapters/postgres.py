from __future__ import annotations

from typing import Any

from .base import Capabilities
from .duckdb import quote_sql_identifier
from .kill import POSTGRES_KILL_DDL
from .pg_catalog import introspect_pg_catalog

try:
    import psycopg
except ImportError:  # pragma: no cover
    psycopg = None


class PostgresAdapter:
    dialect = "postgres"
    capabilities = Capabilities(exception_udf=True)

    def __init__(self, conn) -> None:
        if psycopg is None:
            raise RuntimeError("Postgres support requires psycopg: uv sync --extra postgres")
        self._conn = conn

    @property
    def connection(self):
        return self._conn

    def execute(self, sql: str, params: Any = None):
        cursor = self._conn.cursor()
        try:
            cursor.execute(sql, params)
        except Exception:
            self._conn.rollback()
            raise
        return cursor

    def quote_identifier(self, name: str) -> str:
        return quote_sql_identifier(name)

    def register_kill_function(self) -> None:
        cursor = self._conn.cursor()
        cursor.execute(POSTGRES_KILL_DDL)
        if not self._conn.autocommit:
            self._conn.commit()

    def introspect_catalog(self) -> dict:
        return introspect_pg_catalog(self._conn, dialect=self.dialect)

    def close(self) -> None:
        self._conn.close()
