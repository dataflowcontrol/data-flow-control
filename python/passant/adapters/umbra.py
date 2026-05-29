from __future__ import annotations

from typing import Any

from .base import Capabilities
from .duckdb import quote_sql_identifier
from .pg_catalog import introspect_pg_catalog

try:
    import psycopg
except ImportError:  # pragma: no cover
    psycopg = None


class UmbraAdapter:
    dialect = "umbra"
    capabilities = Capabilities(exception_udf=False)

    def __init__(self, conn: Any) -> None:
        if psycopg is None:
            raise RuntimeError("Umbra support requires psycopg: uv sync --extra postgres")
        self._conn = conn

    @property
    def connection(self):
        return self._conn

    def execute(self, sql: str, params: Any = None):
        cursor = self._conn.cursor()
        cursor.execute(sql, params)
        return cursor

    def quote_identifier(self, name: str) -> str:
        return quote_sql_identifier(name)

    def register_kill_function(self) -> None:
        return

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
        return introspect_pg_catalog(self._conn, dialect=self.dialect)

    def close(self) -> None:
        self._conn.close()
