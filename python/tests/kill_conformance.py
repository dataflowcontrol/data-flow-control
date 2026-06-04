"""Shared KILL resolution scan conformance helpers."""

from __future__ import annotations

import pytest

from data_flow_control import Policy, Resolution
from data_flow_control.adapters.kill import kill_exc_match


def kill_scan_conformance(
    db,
    *,
    table: str = "foo",
    skip_ddl: bool = False,
) -> None:
    """Register a KILL policy and assert violating scan queries abort."""
    if not skip_ddl:
        db.execute(f"CREATE TABLE {table} (id INTEGER)")
        db.execute(f"INSERT INTO {table} VALUES (1)")
    db.register_policy(
        Policy(
            sources=[table],
            constraint=f"max({table}.id) > 1",
            on_fail=Resolution.KILL,
        )
    )
    exc_match = kill_exc_match(db.adapter.dialect)
    with pytest.raises(Exception, match=exc_match):
        db.fetchall(f"SELECT id FROM {table}")
