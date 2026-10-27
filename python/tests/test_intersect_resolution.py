"""Bounded terminal INTERSECT DISTINCT provenance/atomic-resolution contract.

These are acceptance tests for the explicitly supported disjoint-source subset,
not permission to generalize INTERSECT ALL or non-idempotent aggregates.
"""

import duckdb
import pytest
from data_flow_control import PassantRewriteError, Policy, Resolution, RetryViolation, dfc


@pytest.fixture
def intersect_db():
    raw = duckdb.connect()
    raw.execute(
        "CREATE TABLE foo(id INTEGER); CREATE TABLE bar(id INTEGER); "
        "CREATE TABLE absent(id INTEGER); CREATE TABLE target(id INTEGER)"
    )
    conn = dfc(raw)
    try:
        yield raw, conn
    finally:
        conn.close()


def register(conn, sources, constraint, *, resolution=Resolution.RETRY, message="retry", sink=None):
    conn.register_policy(
        Policy(
            sources=sources,
            sink=sink,
            constraint=constraint,
            on_fail=resolution,
            retry_message=message if resolution is Resolution.RETRY else None,
        )
    )


def test_intersect_legacy_multisource_remove_retains_two_and_three(intersect_db):
    raw, conn = intersect_db
    raw.execute("INSERT INTO foo VALUES (1),(2),(3); INSERT INTO bar VALUES (1),(2),(3)")
    register(
        conn,
        ["foo", "bar"],
        "MAX(foo.id) > 1 AND MAX(bar.id) > 1",
        resolution=Resolution.REMOVE,
    )
    cursor = conn.execute("SELECT id FROM foo INTERSECT SELECT id FROM bar ORDER BY id")
    assert [column[0] for column in cursor.description] == ["id"]
    assert cursor.fetchall() == [(2,), (3,)]


def test_intersect_duplicates_do_not_change_public_schema_or_cardinality(intersect_db):
    raw, conn = intersect_db
    raw.execute(
        "INSERT INTO foo VALUES (1),(1),(2),(3); INSERT INTO bar VALUES (1),(1),(1),(2),(4)"
    )
    register(conn, ["foo", "bar"], "MIN(foo.id) > 0 AND MAX(bar.id) > 0")
    cursor = conn.execute('SELECT id AS "Visible Key" FROM foo INTERSECT SELECT id FROM bar')
    assert [column[0] for column in cursor.description] == ["Visible Key"]
    assert sorted(cursor.fetchall()) == [(1,), (2,)]


def test_intersect_null_visible_keys_match_null_safely(intersect_db):
    raw, conn = intersect_db
    raw.execute("INSERT INTO foo VALUES (1),(2); INSERT INTO bar VALUES (3),(4)")
    register(conn, ["foo", "bar"], "MIN(foo.id) = 1 AND MAX(bar.id) = 4")
    cursor = conn.execute(
        'SELECT NULL::INTEGER AS "Nullable Key" FROM foo INTERSECT SELECT NULL::INTEGER FROM bar'
    )
    assert [column[0] for column in cursor.description] == ["Nullable Key"]
    assert cursor.fetchall() == [(None,)]


def test_intersect_unmatched_source_rows_do_not_trigger_retry(intersect_db):
    raw, conn = intersect_db
    raw.execute("INSERT INTO foo VALUES (-1),(2); INSERT INTO bar VALUES (2),(3)")
    register(conn, ["foo"], "MIN(foo.id) > 0", message="unmatched row is not a contributor")
    assert conn.execute("SELECT id FROM foo INTERSECT SELECT id FROM bar").fetchall() == [(2,)]


def test_intersect_source_applicability_requires_each_declared_source(intersect_db):
    raw, conn = intersect_db
    raw.execute("INSERT INTO foo VALUES (2); INSERT INTO bar VALUES (2)")
    register(conn, ["foo", "absent"], "MAX(foo.id) < 0", message="inapplicable without absent")
    assert conn.execute("SELECT id FROM foo INTERSECT SELECT id FROM bar").fetchall() == [(2,)]


@pytest.mark.parametrize(
    "foo_value,bar_value,expected_ids", [(-1, -2, [0, 2]), (1, -2, [0]), (-1, 2, [2])]
)
def test_intersect_retry_collects_actual_ids_and_insert_is_atomic(
    intersect_db, monkeypatch, foo_value, bar_value, expected_ids
):
    raw, conn = intersect_db
    raw.execute("INSERT INTO foo VALUES (?)", [foo_value])
    raw.execute("INSERT INTO bar VALUES (?)", [bar_value])
    raw.execute("INSERT INTO target VALUES (99)")
    register(conn, ["bar"], "MIN(bar.id) > 0", message=" B's exact message ☃\n", sink="target")
    register(
        conn,
        ["foo", "bar"],
        "MAX(foo.id) < 100 AND MAX(bar.id) < 100",
        message="passing",
        sink="target",
    )
    register(conn, ["foo"], "MAX(foo.id) > 0", message="A failed", sink="target")
    submitted = []
    execute = conn.adapter.execute

    def spy(sql, params=None):
        submitted.append(sql)
        return execute(sql, params)

    monkeypatch.setattr(conn.adapter, "execute", spy)
    with pytest.raises(RetryViolation) as caught:
        conn.execute("INSERT INTO target SELECT 7 AS id FROM foo INTERSECT SELECT 7 AS id FROM bar")
    assert [failure.policy_index for failure in caught.value.failures] == expected_ids
    messages = {0: " B's exact message ☃\n", 2: "A failed"}
    assert caught.value.messages == [messages[index] for index in expected_ids]
    assert len(submitted) == 1
    assert raw.execute("SELECT id FROM target").fetchall() == [(99,)]


def test_intersect_scalar_hidden_source_value_still_requires_uniqueness(intersect_db):
    raw, conn = intersect_db
    raw.execute("INSERT INTO foo VALUES (1),(2); INSERT INTO bar VALUES (3)")
    register(conn, ["foo"], "foo.id > 0", message="ambiguous source value")
    with pytest.raises(RetryViolation) as caught:
        conn.execute("SELECT 7 AS id FROM foo INTERSECT SELECT 7 AS id FROM bar")
    assert caught.value.messages == ["ambiguous source value"]


@pytest.mark.parametrize("aggregate", ["COUNT", "SUM", "AVG"])
def test_intersect_non_idempotent_policy_aggregate_is_explicitly_unsupported(
    intersect_db, aggregate
):
    raw, conn = intersect_db
    raw.execute("INSERT INTO foo VALUES (1),(1); INSERT INTO bar VALUES (1)")
    register(conn, ["foo", "bar"], f"{aggregate}(foo.id) > 0 AND MAX(bar.id) > 0")
    with pytest.raises(PassantRewriteError):
        conn.execute("SELECT id FROM foo INTERSECT SELECT id FROM bar")


def test_intersect_all_is_explicitly_unsupported(intersect_db):
    raw, conn = intersect_db
    raw.execute("INSERT INTO foo VALUES (1),(1); INSERT INTO bar VALUES (1)")
    register(conn, ["foo", "bar"], "MAX(foo.id) > 0 AND MAX(bar.id) > 0")
    with pytest.raises(PassantRewriteError):
        conn.execute("SELECT id FROM foo INTERSECT ALL SELECT id FROM bar")


def test_intersect_overlapping_source_provenance_is_explicitly_unsupported(intersect_db):
    raw, conn = intersect_db
    raw.execute("INSERT INTO foo VALUES (1),(1)")
    register(conn, ["foo"], "MAX(foo.id) > 0")
    with pytest.raises(PassantRewriteError):
        conn.execute("SELECT id FROM foo INTERSECT SELECT id FROM foo")


def test_intersect_explicit_distinct_preserves_minmax_hidden_evidence(intersect_db):
    raw, conn = intersect_db
    raw.execute("INSERT INTO foo VALUES (1),(2); INSERT INTO bar VALUES (3),(4)")
    register(conn, ["foo", "bar"], "MIN(foo.id) = 1 AND MAX(bar.id) = 4")
    cursor = conn.execute("SELECT 7 AS id FROM foo INTERSECT DISTINCT SELECT 7 AS id FROM bar")
    assert [column[0] for column in cursor.description] == ["id"]
    assert cursor.fetchall() == [(7,)]


def test_intersect_actual_null_source_value_is_not_absent_provenance(intersect_db):
    raw, conn = intersect_db
    raw.execute("INSERT INTO foo VALUES (NULL); INSERT INTO bar VALUES (1)")
    register(conn, ["foo"], "foo.id > 0", message="present NULL fails")
    with pytest.raises(RetryViolation) as caught:
        conn.execute("SELECT 7 AS id FROM foo INTERSECT SELECT 7 AS id FROM bar")
    assert caught.value.messages == ["present NULL fails"]


@pytest.mark.parametrize("text_value", ["01", "1"])
def test_intersect_preserves_set_operation_common_public_type(text_value):
    raw = duckdb.connect()
    raw.execute("CREATE TABLE numbers(k INTEGER); CREATE TABLE strings(k VARCHAR)")
    raw.execute("INSERT INTO numbers VALUES (1)")
    raw.execute("INSERT INTO strings VALUES (?)", [text_value])
    sql = "SELECT k FROM numbers INTERSECT SELECT k FROM strings"
    original = raw.execute(sql)
    expected_schema = original.description
    expected = original.fetchall()
    with dfc(raw) as conn:
        register(
            conn,
            ["numbers", "strings"],
            "MAX(numbers.k) > 0 AND MAX(strings.k) > '0'",
        )
        actual = conn.execute(sql)
        assert actual.description == expected_schema
        assert actual.fetchall() == expected


def test_intersect_does_not_assume_volatile_policy_extrema_are_idempotent(intersect_db):
    raw, conn = intersect_db
    raw.execute("INSERT INTO foo VALUES (1),(1); INSERT INTO bar VALUES (1),(1)")
    register(conn, ["foo", "bar"], "MAX(foo.id + random()) > 0 AND MAX(bar.id) > 0")
    with pytest.raises(PassantRewriteError, match="purity"):
        conn.execute("SELECT id FROM foo INTERSECT SELECT id FROM bar")
