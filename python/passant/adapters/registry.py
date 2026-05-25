from __future__ import annotations

from typing import Any
from urllib.parse import unquote, urlparse

from .base import Adapter
from .duckdb import DuckDBAdapter

SUPPORTED_DIALECTS = frozenset(
    {"duckdb", "sqlite", "postgres", "clickhouse", "datafusion", "umbra"}
)
IMPLEMENTED_DIALECTS = frozenset(
    {"duckdb", "sqlite", "postgres", "clickhouse", "datafusion", "umbra"}
)


def create_adapter(conn: Any, dialect: str) -> Adapter:
    normalized = dialect.strip().lower()
    if normalized == "postgresql":
        normalized = "postgres"
    if normalized not in SUPPORTED_DIALECTS:
        raise ValueError(f"Unknown dialect: {dialect!r}")
    if normalized not in IMPLEMENTED_DIALECTS:
        raise NotImplementedError(
            f"Dialect {normalized!r} is recognized but not implemented yet. "
            f"Implemented: {', '.join(sorted(IMPLEMENTED_DIALECTS))}"
        )
    if normalized == "duckdb":
        return DuckDBAdapter(conn)
    if normalized == "sqlite":
        from .sqlite import SQLiteAdapter

        return SQLiteAdapter(conn)
    if normalized == "postgres":
        from .postgres import PostgresAdapter

        return PostgresAdapter(conn)
    if normalized == "clickhouse":
        from .clickhouse import ClickHouseAdapter

        return ClickHouseAdapter(conn)
    if normalized == "datafusion":
        from .datafusion import DataFusionAdapter

        return DataFusionAdapter(conn)
    if normalized == "umbra":
        from .umbra import UmbraAdapter

        return UmbraAdapter(conn)
    raise NotImplementedError(f"No adapter factory for dialect {normalized!r}")


def connect(url: str, *, dialect: str | None = None) -> tuple[Any, str]:
    """Open a connection from a URL and return `(connection, dialect)`."""
    parsed = urlparse(url)
    scheme = (parsed.scheme or "").lower()
    if dialect is None:
        if scheme in ("postgresql", "postgres"):
            dialect = "postgres"
        elif scheme == "sqlite":
            dialect = "sqlite"
        elif scheme == "clickhouse":
            dialect = "clickhouse"
        elif scheme == "datafusion":
            dialect = "datafusion"
        elif scheme == "umbra":
            dialect = "umbra"
        elif scheme in ("duckdb", ""):
            dialect = "duckdb"
        else:
            dialect = scheme
    normalized = dialect.lower()
    if normalized == "postgresql":
        normalized = "postgres"

    if normalized == "duckdb":
        import duckdb

        if scheme in ("duckdb", ""):
            path = unquote(parsed.path or "")
            if path in ("", "/:memory:", "/:memory"):
                return duckdb.connect(), normalized
            if path.startswith("/"):
                path = path[1:]
            return duckdb.connect(path or None), normalized
        return duckdb.connect(url), normalized

    if normalized == "sqlite":
        import sqlite3

        if scheme == "sqlite":
            path = unquote(parsed.path or "")
            if path in (":memory:", "/:memory:", "/:memory"):
                return sqlite3.connect(":memory:"), normalized
            if path.startswith("/") and len(path) > 1:
                path = path[1:]
            return sqlite3.connect(path), normalized
        return sqlite3.connect(url), normalized

    if normalized == "postgres":
        try:
            import psycopg
        except ImportError as exc:  # pragma: no cover
            raise RuntimeError("Postgres URLs require psycopg: uv sync --extra postgres") from exc
        return psycopg.connect(url), normalized

    if normalized == "datafusion":
        try:
            import datafusion
        except ImportError as exc:  # pragma: no cover
            raise RuntimeError(
                "DataFusion URLs require datafusion: uv sync --extra datafusion"
            ) from exc
        return datafusion.SessionContext(), normalized

    if normalized == "clickhouse":
        try:
            import clickhouse_connect
        except ImportError as exc:  # pragma: no cover
            raise RuntimeError(
                "ClickHouse URLs require clickhouse-connect: uv sync --extra clickhouse"
            ) from exc
        host = parsed.hostname or "localhost"
        port = parsed.port or 8123
        database = (parsed.path or "/default").lstrip("/") or "default"
        username = unquote(parsed.username) if parsed.username else "default"
        password = unquote(parsed.password) if parsed.password else ""
        client = clickhouse_connect.get_client(
            host=host,
            port=port,
            username=username,
            password=password,
            database=database,
        )
        return client, normalized

    if normalized == "umbra":
        try:
            import psycopg
        except ImportError as exc:  # pragma: no cover
            raise RuntimeError("Umbra URLs require psycopg: uv sync --extra postgres") from exc
        if scheme == "umbra":
            host = parsed.hostname or "127.0.0.1"
            port = parsed.port or 15433
            username = unquote(parsed.username) if parsed.username else "postgres"
            password = unquote(parsed.password) if parsed.password else "postgres"
            database = (parsed.path or "/postgres").lstrip("/") or "postgres"
            url = f"postgresql://{username}:{password}@{host}:{port}/{database}"
            return psycopg.connect(url), normalized
        return psycopg.connect(url), normalized

    raise NotImplementedError(
        f"connect() does not support dialect {normalized!r} yet. "
        f"Use wrap(existing_conn, dialect=...) when you have a driver connection."
    )
