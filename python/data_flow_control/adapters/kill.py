from __future__ import annotations

import re

KILL_MESSAGE = "KILLing due to dfc policy violation"

POSTGRES_KILL_DDL = tuple(
    f"""
CREATE OR REPLACE FUNCTION {name}() RETURNS boolean
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION '{KILL_MESSAGE}';
END;
$$;
""".strip()
    for name in ("kill", "passant_kill")
)

CLICKHOUSE_KILL_DDL = tuple(
    f"CREATE OR REPLACE FUNCTION {name} AS () -> throwIf(1, '{KILL_MESSAGE}')"
    for name in ("kill", "passant_kill")
)


def python_kill() -> bool:
    raise ValueError(KILL_MESSAGE)


def kill_exc_match(dialect: str) -> str | None:
    """Regex for pytest.raises(match=...) when a KILL policy aborts a query."""
    if dialect == "sqlite":
        return "user-defined function raised exception"
    return re.escape(KILL_MESSAGE)
