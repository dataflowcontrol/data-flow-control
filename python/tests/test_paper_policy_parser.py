"""Phase 2: paper-compatible PGN parser and policy model."""

from __future__ import annotations

import pytest

from passant import Policy, Resolution
from passant._rust import parse_policy_to_json


def test_source_alias_without_as():
    policy = Policy.from_pgn("SOURCE Receipts R CONSTRAINT max(Receipts.id) > 0 ON FAIL REMOVE")
    assert policy.sources == ["Receipts"]
    assert policy.source_aliases == {"r": "Receipts"}


def test_sink_alias_without_as():
    policy = Policy.from_pgn(
        "SOURCE Receipts SINK Expenses E CONSTRAINT max(Receipts.id) > 0 ON FAIL REMOVE"
    )
    assert policy.sink == "Expenses"
    assert policy.sink_alias == "E"


def test_dimension_table_aliases():
    policy = Policy.from_pgn(
        "SOURCE Receipts "
        "DIMENSION catalog_users U, catalog_roles R "
        "CONSTRAINT max(Receipts.id) > 0 ON FAIL REMOVE"
    )
    assert policy.dimensions == ["catalog_users", "catalog_roles"]
    assert policy.dimension_aliases == {"u": "catalog_users", "r": "catalog_roles"}


def test_resolution_udf_parsing():
    policy = Policy.from_pgn("SOURCE foo CONSTRAINT max(foo.id) > 0 ON FAIL UDF keep_positive")
    assert policy.on_fail == Resolution.UDF
    assert policy.udf_name == "keep_positive"


def test_resolution_relation_udf_parsing():
    policy = Policy.from_pgn(
        "SINK reports CONSTRAINT max(reports.total) > 0 ON FAIL RELATION UDF abort_batch"
    )
    assert policy.on_fail == Resolution.RELATION_UDF
    assert policy.udf_name == "abort_batch"


def test_rejects_llm_resolution():
    with pytest.raises(ValueError, match="invalid resolution"):
        Policy.from_pgn("SOURCE foo CONSTRAINT max(foo.id) > 0 ON FAIL LLM")


def test_rejects_invalidate_resolution():
    with pytest.raises(ValueError, match="invalid resolution"):
        Policy.from_pgn("SOURCE foo CONSTRAINT max(foo.id) > 0 ON FAIL INVALIDATE")


def test_required_source_must_be_listed_in_sources():
    with pytest.raises(ValueError, match="Required sources must also be listed"):
        Policy(
            sources=["foo"],
            required_sources=["bar"],
            constraint="max(foo.id) > 0",
            on_fail=Resolution.REMOVE,
        )


def test_required_source_valid_when_listed():
    policy = Policy.from_pgn(
        "SOURCE REQUIRED Receipts SINK Expenses CONSTRAINT Receipts.id = Expenses.id ON FAIL REMOVE"
    )
    assert policy.required_sources == ["Receipts"]
    assert policy.sources == ["Receipts"]


def test_parse_policy_json_includes_dimension_fields():
    parsed = parse_policy_to_json(
        "SOURCE foo DIMENSION dim_table D CONSTRAINT max(foo.id) > 0 ON FAIL REMOVE"
    )
    spec = parsed["Pgn"]
    assert spec["dimension_tables"] == ["dim_table"]
    assert spec["dimension_aliases"] == {"d": "dim_table"}
