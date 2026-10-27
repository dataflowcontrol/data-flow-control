"""EXCEPT keeps positive contributors only, after public-key exclusion."""
import duckdb
import pytest

from data_flow_control import PassantRewriteError, Policy, Resolution, RetryViolation, dfc


@pytest.fixture
def db():
    raw = duckdb.connect()
    raw.execute("CREATE TABLE a(x INTEGER, y INTEGER)")
    raw.execute("CREATE TABLE b(x INTEGER)")
    raw.execute("CREATE TABLE target(x INTEGER)")
    raw.execute("CREATE TABLE other(x INTEGER)")
    conn = dfc(raw)
    yield raw, conn
    conn.close()


def policy(conn, sources, constraint, sink="target", message="positive failed"):
    conn.register_policy(Policy(sources=sources, sink=sink, constraint=constraint,
                                on_fail=Resolution.RETRY, retry_message=message))


@pytest.mark.parametrize("query", [
    "SELECT x FROM a EXCEPT SELECT x FROM b",
    "WITH aa AS (SELECT x,y FROM a), bb AS (SELECT x FROM b) SELECT x FROM aa EXCEPT SELECT x FROM bb",
    "SELECT x FROM a WHERE y < 10 EXCEPT SELECT x FROM b",
])
def test_removed_violation_never_resolves(db, query):
    raw, conn = db
    raw.execute("INSERT INTO a VALUES (-1,-1),(1,1),(1,1)")
    raw.execute("INSERT INTO b VALUES (-1)")
    policy(conn, ["a"], "a.y > 0")
    conn.execute("INSERT INTO target " + query)
    assert raw.execute("SELECT * FROM target").fetchall() == [(1,)]


def test_surviving_violation_exact_identity_and_atomicity(db):
    raw, conn = db
    raw.execute("INSERT INTO a VALUES (-1,-1),(1,1)")
    raw.execute("INSERT INTO target VALUES (99)")
    policy(conn, ["a"], "a.y > -2", message="passing must not appear")
    policy(conn, ["a"], "a.y > 0", message=" Don't leak ☃\n")
    with pytest.raises(RetryViolation) as error:
        conn.execute("INSERT INTO target SELECT x FROM a EXCEPT SELECT x FROM b")
    assert [(f.policy_index, f.message) for f in error.value.failures] == [(1, " Don't leak ☃\n")]
    assert raw.execute("SELECT * FROM target").fetchall() == [(99,)]


@pytest.mark.parametrize("sources,constraint", [(["b"], "b.x > 0"), (["a", "b"], "MAX(a.x) > MAX(b.x)")])
def test_negative_dependency_is_policy_specific_preexecution_error(db, sources, constraint):
    raw, conn = db
    policy(conn, ["a"], "a.y > 0")
    policy(conn, sources, constraint)
    with pytest.raises(PassantRewriteError, match=r"policy 1 .*negative-side SOURCE b.*EXCEPT"):
        conn.execute("INSERT INTO target SELECT x FROM a EXCEPT SELECT x FROM b")
    assert raw.execute("SELECT * FROM target").fetchall() == []


@pytest.mark.parametrize("cte", ["bb", "b"])
def test_negative_cte_dependency_is_identified(db, cte):
    _, conn = db
    policy(conn, ["b"], "b.x > 0")
    with pytest.raises(PassantRewriteError, match=r"policy 0 .*negative-side SOURCE b"):
        conn.execute(f"INSERT INTO target WITH {cte} AS (SELECT x FROM b) SELECT x FROM a EXCEPT SELECT x FROM {cte}")


def test_unrelated_sink_negative_policy_is_inapplicable(db):
    raw, conn = db
    raw.execute("INSERT INTO a VALUES (1,1)")
    policy(conn, ["b"], "b.x > 0", sink="other")
    policy(conn, ["a"], "a.y > 0")
    conn.execute("INSERT INTO target SELECT x FROM a EXCEPT SELECT x FROM b")
    assert raw.execute("SELECT * FROM target").fetchall() == [(1,)]


def test_null_public_key_excluded_with_set_semantics(db):
    raw, conn = db
    raw.execute("INSERT INTO a VALUES (NULL,-1),(1,1)")
    raw.execute("INSERT INTO b VALUES (NULL)")
    policy(conn, ["a"], "a.y > 0")
    conn.execute("INSERT INTO target SELECT x FROM a EXCEPT SELECT x FROM b")
    assert raw.execute("SELECT * FROM target").fetchall() == [(1,)]


@pytest.mark.parametrize("ys,violates", [("(7,1),(7,2)", True), ("(7,1),(7,1)", False)])
def test_duplicate_positive_contributors_preserve_global_uniqueness(db, ys, violates):
    raw, conn = db
    raw.execute("INSERT INTO a VALUES " + ys)
    policy(conn, ["a"], "a.y > 0 OR TRUE")
    if violates:
        with pytest.raises(RetryViolation):
            conn.execute("INSERT INTO target SELECT x FROM a EXCEPT SELECT x FROM b")
        assert raw.execute("SELECT * FROM target").fetchall() == []
    else:
        conn.execute("INSERT INTO target SELECT x FROM a EXCEPT SELECT x FROM b")
        assert raw.execute("SELECT * FROM target").fetchall() == [(7,)]


@pytest.mark.parametrize("tail", ["ORDER BY x DESC LIMIT 1", "ORDER BY x OFFSET 1", "LIMIT 0"])
def test_outer_candidate_limit_precedes_effects(db, tail):
    raw, conn = db
    raw.execute("INSERT INTO a VALUES (-1,-1),(1,1)")
    policy(conn, ["a"], "a.y > 0")
    conn.execute("INSERT INTO target SELECT x FROM a EXCEPT SELECT x FROM b " + tail)
    assert raw.execute("SELECT * FROM target").fetchall() == ([] if tail == "LIMIT 0" else [(1,)])


@pytest.mark.parametrize("query", [
    "SELECT x FROM a EXCEPT ALL SELECT x FROM b",
    "(SELECT x FROM a EXCEPT SELECT x FROM b) UNION ALL SELECT x FROM b",
    "SELECT x FROM (SELECT x FROM a EXCEPT SELECT x FROM b) q WHERE x > 0",
    "SELECT x FROM a EXCEPT SELECT x FROM b EXCEPT SELECT x FROM b",
])
def test_unproven_nested_or_bag_shape_rejects_before_execution(db, query):
    raw, conn = db
    policy(conn, ["a"], "a.y > 0")
    with pytest.raises(PassantRewriteError):
        conn.execute("INSERT INTO target " + query)
    assert raw.execute("SELECT * FROM target").fetchall() == []


def test_public_common_type_not_raw_comparison_type(db):
    raw, conn = db
    raw.execute("INSERT INTO a VALUES (1,1)")
    raw.execute("INSERT INTO b VALUES (1)")
    policy(conn, ["a"], "a.y > 0", sink=None)
    query = 'SELECT x AS "quoted label" FROM a EXCEPT SELECT \'01\' FROM b'
    expected = raw.execute(query)
    expected_schema, expected_rows = expected.description, expected.fetchall()
    result = conn.execute(query)
    assert result.description == expected_schema
    assert result.fetchall() == expected_rows == [("1",)]


def test_missing_ordinary_source_makes_negative_dependency_inapplicable(db):
    raw, conn = db
    raw.execute("INSERT INTO a VALUES (1,1)")
    # 'other' is catalog-bound but has no contributor in this statement.
    policy(conn, ["b", "other"], "MAX(b.x) > MAX(other.x)", sink=None)
    assert conn.execute("SELECT x FROM a EXCEPT SELECT x FROM b").fetchall() == [(1,)]


@pytest.mark.parametrize("constraint", ["COUNT(a.y) = 3", "SUM(a.y) = 5", "AVG(a.y) > 1.6"])
def test_positive_duplicate_aggregate_multiplicity_is_preserved(db, constraint):
    raw, conn = db
    raw.execute("INSERT INTO a VALUES (7,1),(7,2),(7,2),(8,-1)")
    raw.execute("INSERT INTO b VALUES (8)")
    policy(conn, ["a"], constraint)
    conn.execute("INSERT INTO target SELECT x FROM a EXCEPT SELECT x FROM b")
    assert raw.execute("SELECT * FROM target").fetchall() == [(7,)]


def test_sink_only_policy_checks_surviving_public_output(db):
    raw, conn = db
    raw.execute("INSERT INTO a VALUES (-1,-1),(1,1)")
    raw.execute("INSERT INTO b VALUES (-1)")
    policy(conn, [], "target.x > 0")
    conn.execute("INSERT INTO target SELECT x FROM a EXCEPT SELECT x FROM b")
    assert raw.execute("SELECT * FROM target").fetchall() == [(1,)]
