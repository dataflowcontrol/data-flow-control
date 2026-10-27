"""DIMENSION output names cannot hide unbound physical dependencies."""

import duckdb
import pytest
from data_flow_control import PassantRewriteError, Policy, dfc


@pytest.mark.parametrize(
    "query",
    [
        "SELECT missing.id AS id FROM missing",
        "SELECT u.missing AS id FROM users u",
        "SELECT u.id FROM users u WHERE u.missing > 0",
        "SELECT u.id FROM users u JOIN roles r ON u.id=r.missing",
        "SELECT u.id FROM users u ORDER BY missing",
        "SELECT id FROM users u JOIN roles r ON u.id=r.id",
        "SELECT a.id AS id FROM users a JOIN roles b ON c.id=a.id JOIN users c ON TRUE",
    ],
)
def test_dimension_dependency_error_is_atomic(query):
    raw = duckdb.connect()
    raw.execute(
        "CREATE TABLE source(x INT); CREATE TABLE users(id INT); CREATE TABLE roles(id INT)"
    )
    with dfc(raw) as conn:
        good = Policy.from_pgn("SOURCE source CONSTRAINT source.x > 0 ON FAIL REMOVE")
        bad = Policy.from_pgn(
            f"SOURCE source DIMENSION ({query}) d CONSTRAINT source.x > 0 AND d.id > 0 ON FAIL REMOVE"
        )
        with pytest.raises(PassantRewriteError):
            conn.register_policies([good, bad])
        assert conn.policies() == []
        conn.register_policy(good)
        assert conn.planner.policy_by_index(0) is not None
        assert conn.planner.policy_by_index(1) is None


@pytest.mark.parametrize(
    "query",
    [
        "SELECT u.id FROM users u WHERE u.id > 0",
        "SELECT u.id AS id FROM users u JOIN roles r ON u.id=r.id ORDER BY id",
        "SELECT MAX(id) AS id FROM users HAVING MAX(id)>0",
        "SELECT 1 AS id",
        "SELECT a.id AS id FROM users a JOIN roles b ON b.id=a.id JOIN users c ON c.id=b.id",
    ],
)
def test_dimension_explicit_dependencies_register(query):
    raw = duckdb.connect()
    raw.execute(
        "CREATE TABLE source(x INT); CREATE TABLE users(id INT); CREATE TABLE roles(id INT)"
    )
    with dfc(raw) as conn:
        conn.register_policy(
            Policy.from_pgn(
                f"SOURCE source DIMENSION ({query}) d CONSTRAINT source.x > 0 AND d.id > 0 ON FAIL REMOVE"
            )
        )
        assert len(conn.policies()) == 1


@pytest.mark.parametrize(
    "query",
    [
        "WITH d AS (SELECT id FROM users) SELECT id FROM d",
        "SELECT d.id FROM (SELECT id FROM users) d",
        "SELECT (SELECT MAX(id) FROM users) AS id",
        "SELECT u.id FROM users u JOIN roles r USING(id)",
    ],
)
def test_unimplemented_dimension_scopes_are_explicit_not_silently_unbound(query):
    raw = duckdb.connect()
    raw.execute(
        "CREATE TABLE source(x INT); CREATE TABLE users(id INT); CREATE TABLE roles(id INT)"
    )
    with dfc(raw) as conn:
        policy = Policy.from_pgn(
            f"SOURCE source DIMENSION ({query}) d CONSTRAINT source.x > 0 AND d.id > 0 ON FAIL REMOVE"
        )
        with pytest.raises(PassantRewriteError, match="DIMENSION catalog binding"):
            conn.register_policy(policy)
        assert conn.policies() == []
