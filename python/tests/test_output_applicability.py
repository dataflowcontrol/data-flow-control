"""No-SINK _OUTPUT_ policies require every public output attribute."""

import duckdb
import pytest
from data_flow_control import Policy, Resolution, RetryViolation, dfc


def connection(constraint="_OUTPUT_.x > 0", sources=("a",)):
    raw = duckdb.connect()
    raw.execute('CREATE TABLE a(x INT, y INT, "Odd Name" INT)')
    raw.execute("INSERT INTO a VALUES (-1, 2, -3), (NULL, 4, NULL)")
    conn = dfc(raw)
    conn.register_policy(
        Policy(
            sources=list(sources),
            constraint=constraint,
            on_fail=Resolution.RETRY,
            retry_message="actual output failure",
        )
    )
    return conn


@pytest.mark.parametrize(
    "query",
    [
        "SELECT y FROM a ORDER BY y",
        "SELECT x AS hidden FROM a",
        "WITH c AS (SELECT x FROM a) SELECT x AS hidden FROM c",
        "SELECT d.x AS hidden FROM (SELECT x FROM a) d",
        "SELECT y FROM a UNION ALL SELECT y FROM a",
    ],
)
def test_missing_public_output_skips_whole_policy(query):
    with connection() as conn:
        conn.execute(query).fetchall()


@pytest.mark.parametrize(
    "query",
    [
        "SELECT x FROM a",
        'SELECT x AS "X" FROM a',
        "SELECT * FROM a",
        "SELECT a.* FROM a",
        "SELECT -y AS x FROM a",
        "WITH c AS (SELECT x AS z FROM a) SELECT z AS x FROM c",
        "SELECT z AS x FROM (SELECT x AS z FROM a) d",
        "SELECT x FROM a UNION ALL SELECT x FROM a",
    ],
)
def test_present_public_output_enforces_exact_policy(query):
    with connection() as conn:
        with pytest.raises(RetryViolation) as caught:
            conn.execute(query)
        assert caught.value.messages == ["actual output failure"]
        assert [f.policy_index for f in caught.value.failures] == [0]


def test_all_referenced_attributes_required_even_when_other_predicate_fails():
    with connection("_OUTPUT_.x > 0 AND _OUTPUT_.y > 0") as conn:
        assert conn.execute("SELECT x FROM a").fetchall() == [(-1,), (None,)]
        with pytest.raises(RetryViolation):
            conn.execute("SELECT x,y FROM a")


def test_actual_null_column_is_present_not_skipped():
    with connection() as conn:
        with pytest.raises(RetryViolation):
            conn.execute("SELECT x FROM a WHERE x IS NULL")


def test_quoted_output_identifier_and_qualified_wildcard():
    with connection('_OUTPUT_."Odd Name" > 0') as conn:
        conn.execute('SELECT "Odd Name" AS renamed FROM a').fetchall()
        with pytest.raises(RetryViolation):
            conn.execute('SELECT "source alias".* FROM a AS "source alias"')


def test_missing_policy_does_not_suppress_other_actual_failure_or_shift_id():
    with connection("_OUTPUT_.missing > 0") as conn:
        conn.register_policy(
            Policy(
                sources=["a"],
                constraint="_OUTPUT_.x > 0",
                on_fail=Resolution.RETRY,
                retry_message="second actual",
            )
        )
        with pytest.raises(RetryViolation) as caught:
            conn.execute("SELECT x FROM a")
        assert caught.value.messages == ["second actual"]
        assert [f.policy_index for f in caught.value.failures] == [1]


@pytest.mark.parametrize("resolution", [Resolution.KILL, Resolution.REMOVE])
def test_missing_output_skips_whole_policy_for_other_resolutions(resolution):
    with connection("TRUE") as conn:
        conn.register_policy(
            Policy(sources=["a"], constraint="_OUTPUT_.missing > 0 AND FALSE", on_fail=resolution)
        )
        assert conn.execute("SELECT y FROM a ORDER BY y").fetchall() == [(2,), (4,)]


def test_no_sink_insert_uses_target_output_names_not_private_select_aliases():
    with connection() as conn:
        conn.execute("CREATE TABLE good_target(y INT)")
        conn.execute("CREATE TABLE checked_target(x INT)")
        conn.execute("INSERT INTO good_target SELECT x FROM a")
        assert conn.execute("SELECT * FROM good_target").fetchall() == [(-1,), (None,)]
        with pytest.raises(RetryViolation):
            conn.execute("INSERT INTO checked_target SELECT x AS private_alias FROM a")
        assert conn.execute("SELECT * FROM checked_target").fetchall() == []


def test_output_policy_is_not_resolved_before_outer_filter():
    with connection() as conn:
        assert (
            conn.execute("WITH c AS (SELECT x FROM a) SELECT x FROM c WHERE x > 0").fetchall() == []
        )


def test_output_presence_not_changed_by_policy_instrumentation_hidden_columns():
    with connection("_OUTPUT_.__passant_policy_pass > 0") as conn:
        conn.register_policy(Policy(sources=["a"], constraint="TRUE", on_fail=Resolution.REMOVE))
        assert conn.execute("SELECT y FROM a ORDER BY y").fetchall() == [(2,), (4,)]


def test_output_attributes_inside_or_and_functions_still_all_required():
    with connection("FALSE OR COALESCE(_OUTPUT_.x, -1) > 0 OR _OUTPUT_.missing > 0") as conn:
        assert conn.execute("SELECT x FROM a").fetchall() == [(-1,), (None,)]
        with pytest.raises(RetryViolation):
            conn.execute("SELECT x, -1 AS missing FROM a")


@pytest.mark.parametrize(
    "query",
    [
        "WITH RECURSIVE c AS (SELECT y FROM a UNION ALL SELECT y + 1 FROM c WHERE y < 0) SELECT y FROM c",
        "SELECT (SELECT 1) AS unrelated FROM a",
    ],
)
def test_proven_missing_output_skips_before_unneeded_provenance_binding(query):
    with connection() as conn:
        assert len(conn.execute(query).fetchall()) == 2


def test_final_aggregate_output_alias_and_having_candidate_scope():
    with connection() as conn:
        assert conn.execute("SELECT SUM(y) AS x FROM a").fetchall() == [(6,)]
        assert conn.execute("SELECT SUM(y) AS unrelated FROM a").fetchall() == [(6,)]
        assert conn.execute("SELECT SUM(x) AS x FROM a HAVING SUM(x) > 0").fetchall() == []
        with pytest.raises(RetryViolation):
            conn.execute("SELECT SUM(x) AS x FROM a")
