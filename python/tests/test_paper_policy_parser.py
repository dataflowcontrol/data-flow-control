"""Paper PGN parser and policy model."""

from __future__ import annotations

import pytest

from data_flow_control import Policy, Resolution
from data_flow_control._rust import parse_policy_to_json


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


def test_constraint_ignores_on_fail_in_string_literal():
    policy = Policy.from_pgn("SOURCE foo CONSTRAINT foo.status = 'ON FAIL' ON FAIL REMOVE")
    assert policy.constraint == "foo.status = 'ON FAIL'"


def test_constraint_ignores_description_in_string_literal():
    policy = Policy.from_pgn("SOURCE foo CONSTRAINT foo.col = 'DESCRIPTION foo' ON FAIL REMOVE")
    assert policy.constraint == "foo.col = 'DESCRIPTION foo'"


def test_dimension_commas_inside_subquery_do_not_split_list():
    policy = Policy.from_pgn(
        "SOURCE foo "
        "DIMENSION (SELECT id FROM t WHERE x IN (1, 2)) d, catalog_roles r "
        "CONSTRAINT max(foo.id) > 0 ON FAIL REMOVE"
    )
    assert "d" in policy.dimension_queries
    assert policy.dimension_aliases["r"] == "catalog_roles"


def test_constraint_ignores_on_fail_in_quoted_identifier():
    parsed = parse_policy_to_json(r'SOURCE foo CONSTRAINT "ON FAIL" = 1 ON FAIL REMOVE')
    assert parsed["Pgn"]["constraint"] == '"ON FAIL" = 1'
