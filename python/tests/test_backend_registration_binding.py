"""DuckDB preflight binds typed SQL without evaluating policies or dispatching effects."""

import duckdb
import pytest

from data_flow_control import PassantRewriteError, Policy, dfc


@pytest.fixture
def connection():
    raw = duckdb.connect()
    raw.execute(
        "CREATE TABLE source(x INTEGER, label VARCHAR); "
        "CREATE TABLE lookup_correct(id INTEGER, val VARCHAR); "
        "CREATE TABLE target(y INTEGER); "
        "INSERT INTO source VALUES (1, 's'); "
        "INSERT INTO lookup_correct VALUES (2, 'l')"
    )
    with dfc(raw) as conn:
        yield conn


def _policy(constraint, *, dimension="SELECT id FROM lookup_correct"):
    return Policy.from_pgn(
        f"SOURCE source AS s SINK target AS t DIMENSION ({dimension}) AS d "
        f"CONSTRAINT {constraint} ON FAIL REMOVE"
    )


@pytest.mark.parametrize(
    ("constraint", "error"),
    [
        ("EXISTS (SELECT 1 FROM lookup_correct AS l WHERE l.missing = t.y)", "missing"),
        ("EXISTS (SELECT 1 FROM lookup_correct AS l WHERE l.val + 1 = t.y)", "VARCHAR"),
        ("EXISTS (SELECT 1 FROM lookup_correct AS l WHERE lower(l.id) = l.val)", "lower"),
        ("1", "expected boolean"),
    ],
)
def test_constraint_preflight_rejects_invalid_backend_bindings(connection, constraint, error):
    with pytest.raises(PassantRewriteError, match=error):
        connection.register_policy(_policy(constraint))
    assert connection.policies() == []


def test_backend_binds_dimension_even_if_constraint_does_not_reference_it(connection):
    policy = _policy("TRUE", dimension="SELECT lower(id) AS id FROM lookup_correct")
    with pytest.raises(PassantRewriteError, match=r"DIMENSION 'd' failed.*lower"):
        connection.register_policy(policy)
    assert connection.policies() == []


def test_backend_alias_and_aggregate_witness_types(connection):
    policy = _policy(
        "MAX(s.x) > d.id AND t.y = _OUTPUT_.y "
        "AND EXISTS (SELECT 1 FROM lookup_correct AS u WHERE u.id = t.y)"
    )
    connection.register_policy(policy)
    assert connection.last_policy_registration_validation == [
        {"constraint": "duckdb bound", "dimensions": {"d": "duckdb bound"}}
    ]


def test_backend_batch_failure_keeps_store_and_prior_report_atomic(connection):
    good = _policy("MAX(s.x) > d.id AND t.y > 0")
    bad = _policy("EXISTS (SELECT 1 FROM lookup_correct AS u WHERE u.bad = t.y)")
    connection.register_policy(good)
    prior = connection.last_policy_registration_validation
    with pytest.raises(PassantRewriteError, match="registration binding"):
        connection.register_policies([good, bad])
    assert len(connection.policies()) == 1
    assert connection.planner.policy_by_index(1) is None
    assert connection.last_policy_registration_validation == prior


def test_live_python_udf_never_calls_even_in_binder_evaluated_positions(connection):
    calls = []

    def probe(value):
        calls.append(value)
        return value

    connection.raw_connection.create_function("registration_probe", probe, ["INTEGER"], "INTEGER")
    policies = [
        _policy("registration_probe(1) > 0"),
        _policy("EXISTS (SELECT 1 LIMIT registration_probe(1))"),
        _policy("EXISTS (SELECT 1 FROM UNNEST([registration_probe(1)]))"),
    ]
    connection.register_policies(policies)
    assert calls == []
    assert all(
        "static only:" in report["constraint"]
        for report in connection.last_policy_registration_validation
    )
    assert connection.raw_connection.execute("SELECT count(*) FROM target").fetchone() == (0,)
    assert connection.raw_connection.execute("SELECT count(*) FROM source").fetchone() == (1,)


def test_table_function_is_not_run_at_registration(connection):
    policy = _policy("EXISTS (SELECT 1 FROM read_csv_auto('not-present.csv'))")
    connection.register_policy(policy)
    assert "table function" in connection.last_policy_registration_validation[0]["constraint"]


def test_late_bound_output_and_dimension_names_are_not_fabricated(connection):
    policy = Policy.from_pgn(
        "SOURCE source CONSTRAINT _OUTPUT_.query_defined > 0 ON FAIL REMOVE"
    )
    connection.register_policy(policy)
    status = connection.last_policy_registration_validation[0]["constraint"]
    assert status == "static only: _OUTPUT_ columns depend on a future query"
    with pytest.raises(PassantRewriteError, match="subquery table 'd'"):
        connection.register_policy(_policy("EXISTS (SELECT 1 FROM d)"))
    assert len(connection.policies()) == 1
