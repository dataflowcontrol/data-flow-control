from __future__ import annotations

import re
from typing import Any

import duckdb
import pytest

from data_flow_control import Policy, dfc
from data_flow_control.adapters.kill import KILL_MESSAGE, kill_exc_match


def make_conn() -> Any:
    return dfc(duckdb.connect())


def register_pgn(conn: Any, policy_text: str) -> Policy:
    policy = Policy.from_pgn(policy_text)
    conn.register_policy(policy)
    return policy


def assert_kill(conn: Any, sql: str, *, dialect: str = "duckdb") -> None:
    pattern = kill_exc_match(dialect)
    with pytest.raises(Exception, match=pattern or re.escape(KILL_MESSAGE)):
        conn.execute(sql)
