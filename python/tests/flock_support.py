"""Helpers for optional Flock DuckDB extension tests."""

from __future__ import annotations

import os
from typing import TYPE_CHECKING

import duckdb

if TYPE_CHECKING:
    from passant.connection import Connection

_FLOCK_LOADED = False


def flock_available() -> bool:
    """Return True when the Flock community extension can be installed and loaded."""
    global _FLOCK_LOADED
    if _FLOCK_LOADED:
        return True
    try:
        conn = duckdb.connect()
        conn.execute("INSTALL flock FROM community")
        conn.execute("LOAD flock")
        conn.close()
        _FLOCK_LOADED = True
        return True
    except Exception:
        return False


def load_flock(conn: duckdb.DuckDBPyConnection) -> None:
    """Install and load Flock on an existing DuckDB connection."""
    conn.execute("INSTALL flock FROM community")
    conn.execute("LOAD flock")


def flock_openai_configured() -> bool:
    """True when credentials for optional Flock execution tests are present."""
    return bool(os.environ.get("OPENAI_API_KEY") or os.environ.get("FLOCK_OPENAI_API_KEY"))


def configure_flock_openai_secret(conn: duckdb.DuckDBPyConnection) -> None:
    """Register DuckDB OpenAI secret from environment for Flock LLM calls."""
    api_key = os.environ.get("OPENAI_API_KEY") or os.environ.get("FLOCK_OPENAI_API_KEY")
    if not api_key:
        raise RuntimeError("OPENAI_API_KEY or FLOCK_OPENAI_API_KEY is required")
    conn.execute(
        "CREATE OR REPLACE SECRET (TYPE OPENAI, API_KEY ?)",
        [api_key],
    )


def ensure_flock_connection(conn: Connection) -> None:
    """Load Flock on the underlying DuckDB connection."""
    load_flock(conn.raw_connection)
