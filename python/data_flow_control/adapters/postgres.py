from __future__ import annotations

import json
from typing import Any

from ..errors import _NativeRetrySignal
from .base import Capabilities
from .duckdb import quote_sql_identifier
from .kill import POSTGRES_KILL_DDL
from .pg_catalog import introspect_pg_aggregates, introspect_pg_catalog

try:
    import psycopg
except ImportError:  # pragma: no cover
    psycopg = None


_RETRY_SQLSTATE = "PDR01"
_RETRY_KIND = "dfc.retry.v1"
_RETRY_DDL = f"""
CREATE OR REPLACE FUNCTION passant_retry_many(policy_ids text) RETURNS boolean
LANGUAGE plpgsql VOLATILE
AS $$
BEGIN
    IF policy_ids IS NULL OR policy_ids !~ '^[0-9]+(,[0-9]+)*$' THEN
        RAISE EXCEPTION 'Invalid native RETRY policy identity list';
    END IF;
    RAISE EXCEPTION USING
        ERRCODE = '{_RETRY_SQLSTATE}',
        MESSAGE = 'Native RETRY policy signal',
        DETAIL = json_build_object('kind', '{_RETRY_KIND}', 'policy_ids', policy_ids)::text;
END;
$$;
""".strip()


def _retry_failure(exc: Exception) -> tuple[tuple[int, None], ...] | None:
    """Decode our structured server diagnostic, never a displayed error string."""
    if not isinstance(exc, psycopg.Error) or exc.sqlstate != _RETRY_SQLSTATE:
        return None
    try:
        payload = json.loads(exc.diag.message_detail)
        if not isinstance(payload, dict) or payload.get("kind") != _RETRY_KIND:
            return None
        ids = payload.get("policy_ids")
        parts = ids.split(",") if isinstance(ids, str) else []
        if not parts or any(not part or not part.isascii() or not part.isdigit() for part in parts):
            return None
        return tuple((index, None) for index in sorted({int(part) for part in parts}))
    except (TypeError, ValueError):
        return None


class PostgresAdapter:
    dialect = "postgres"
    capabilities = Capabilities(exception_udf=True, retry_resolution=True)

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
        except Exception as exc:
            self._conn.rollback()
            failure = _retry_failure(exc)
            if failure is not None:
                raise _NativeRetrySignal(failure) from exc
            raise
        return cursor

    def quote_identifier(self, name: str) -> str:
        return quote_sql_identifier(name)

    def register_kill_function(self) -> None:
        cursor = self._conn.cursor()
        for statement in POSTGRES_KILL_DDL:
            cursor.execute(statement)
        if not self._conn.autocommit:
            self._conn.commit()

    def register_retry_function(self) -> None:
        # Same installation/transaction contract as the existing KILL helper.
        cursor = self._conn.cursor()
        cursor.execute(_RETRY_DDL)
        if not self._conn.autocommit:
            self._conn.commit()

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

    def introspect_aggregate_functions(self) -> list[dict]:
        return introspect_pg_aggregates(self._conn)

    def close(self) -> None:
        self._conn.close()
