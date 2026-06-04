"""Helpers for optional extended_duckdb UI integration tests."""

from __future__ import annotations

import os
from pathlib import Path

import duckdb

_REPO_ROOT = Path(__file__).resolve().parents[3]
_DEFAULT_EXTENSION = (
    _REPO_ROOT
    / "extended_duckdb"
    / "build"
    / "release"
    / "extension"
    / "external"
    / "external.duckdb_extension"
)


def ui_extension_path() -> str | None:
    """Return the external.duckdb_extension path when it exists."""
    env = os.environ.get("PASSANT_EXTERNAL_EXTENSION")
    if env:
        return env if Path(env).is_file() else None
    if _DEFAULT_EXTENSION.is_file():
        return str(_DEFAULT_EXTENSION)
    return None


def ui_extension_available() -> bool:
    path = ui_extension_path()
    if path is None:
        return False
    try:
        conn = duckdb.connect(config={"allow_unsigned_extensions": "true"})
        conn.execute(f"LOAD {path!r}")
        conn.close()
        return True
    except Exception:
        return False


def duckdb_with_ui_extension() -> duckdb.DuckDBPyConnection:
    path = ui_extension_path()
    if path is None:
        raise RuntimeError(
            "extended_duckdb extension not found; build extended_duckdb or set "
            "PASSANT_EXTERNAL_EXTENSION"
        )
    conn = duckdb.connect(config={"allow_unsigned_extensions": "true"})
    conn.execute(f"LOAD {path!r}")
    return conn
