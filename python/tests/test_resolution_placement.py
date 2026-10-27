"""Acceptance regressions for resolution placement at final candidate boundaries.

These assert semantics, not a particular rewrite spelling or expression call
count. Equal-priority multi-UDF ordering and UI precedence are not specified.
"""

import duckdb
import pytest
from data_flow_control import PassantRewriteError, Policy, Resolution, RetryViolation, dfc
from data_flow_control.adapters.kill import KILL_MESSAGE


@pytest.fixture
def placement_db():
    raw = duckdb.connect()
    for table in ("a", "b", "target"):
        raw.execute(f"CREATE TABLE {table} (x INTEGER)")
    conn = dfc(raw)
    yield raw, conn
    conn.close()


def add_policy(conn, table, constraint, resolution=Resolution.RETRY, message=None, udf=None):
    conn.register_policy(
        Policy(
            sources=[table],
            sink="target",
            constraint=constraint,
            on_fail=resolution,
            retry_message=message if resolution is Resolution.RETRY else None,
            udf_name=udf,
        )
    )


def assert_failures(error, expected):
    assert [(f.policy_index, f.message) for f in error.failures] == expected
    assert error.messages == [message for _, message in expected]
    assert all(f.policy is not None and f.policy.retry_message == f.message for f in error.failures)


def target_rows(raw):
    return raw.execute("SELECT x FROM target ORDER BY x").fetchall()


@pytest.mark.parametrize(
    "query",
    [
        "SELECT x FROM a UNION ALL SELECT x FROM b",
        "WITH aa AS (SELECT x FROM a), bb AS (SELECT x FROM b) SELECT aa.x FROM aa CROSS JOIN bb",
    ],
)
def test_non_splittable_retry_collects_all_actual_failures_across_scopes(placement_db, query):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1)")
    raw.execute("INSERT INTO b VALUES (-2)")
    add_policy(conn, "b", "b.x > 0", message=" B's exact message ☃\n")
    add_policy(conn, "a", "a.x > 0", message="A failed")
    with pytest.raises(RetryViolation) as caught:
        conn.execute(f"INSERT INTO target {query}")
    assert_failures(caught.value, [(0, " B's exact message ☃\n"), (1, "A failed")])
    assert caught.value.message is None
    assert target_rows(raw) == []


@pytest.mark.parametrize(
    "query",
    [
        "WITH aa AS (SELECT x FROM a) SELECT aa.x FROM aa JOIN b ON aa.x = b.x",
        "WITH aa AS (SELECT x FROM a) SELECT x FROM aa WHERE x > 0",
        "WITH aa AS (SELECT x FROM a) SELECT x FROM aa ORDER BY x DESC LIMIT 1",
        "SELECT x FROM a UNION ALL SELECT x FROM b ORDER BY x DESC LIMIT 1",
    ],
)
def test_rows_eliminated_by_query_do_not_trigger_final_retry(placement_db, query):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1), (1)")
    raw.execute("INSERT INTO b VALUES (1)")
    add_policy(conn, "a", "a.x > 0", message="Eliminated row must not fail")
    conn.execute(f"INSERT INTO target {query}")
    assert target_rows(raw) == [(1,)]


def test_same_policy_identity_in_multiple_branches_is_reported_once(placement_db):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1), (-2)")
    add_policy(conn, "a", "a.x > 0", message="One policy, multiple branches")
    with pytest.raises(RetryViolation) as caught:
        conn.execute("INSERT INTO target SELECT x FROM a UNION ALL SELECT x FROM a")
    assert_failures(caught.value, [(0, "One policy, multiple branches")])
    assert target_rows(raw) == []


@pytest.mark.parametrize(
    "operator, expected",
    [
        ("UNION", [(1,), (2,), (3,)]),
        ("UNION ALL", [(1,), (1,), (2,), (2,), (3,)]),
    ],
)
def test_placement_preserves_visible_union_duplicate_semantics(placement_db, operator, expected):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (1), (1), (2)")
    raw.execute("INSERT INTO b VALUES (2), (3)")
    add_policy(conn, "a", "a.x > 0", message="A positive")
    add_policy(conn, "b", "b.x > 0", message="B positive")
    conn.execute(f"INSERT INTO target SELECT x FROM a {operator} SELECT x FROM b")
    assert target_rows(raw) == expected


def test_deferred_policy_retains_nonprojected_source_attribute(placement_db):
    raw, conn = placement_db
    raw.execute("ALTER TABLE a ADD COLUMN allowed BOOLEAN")
    raw.execute("ALTER TABLE b ADD COLUMN allowed BOOLEAN")
    raw.execute("INSERT INTO a VALUES (1, false)")
    raw.execute("INSERT INTO b VALUES (2, false)")
    add_policy(conn, "a", "a.allowed", message="A hidden attribute failed")
    add_policy(conn, "b", "b.allowed", message="B hidden attribute failed")
    with pytest.raises(RetryViolation) as caught:
        conn.execute("INSERT INTO target SELECT x FROM a UNION ALL SELECT x FROM b")
    assert_failures(
        caught.value, [(0, "A hidden attribute failed"), (1, "B hidden attribute failed")]
    )
    assert target_rows(raw) == []


def test_aggregate_policy_is_not_replaced_by_or_of_per_input_failures(placement_db):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1), (1)")
    add_policy(conn, "a", "MAX(a.x) > 0", message="Aggregate must be positive")
    conn.execute("INSERT INTO target SELECT MAX(x) AS x FROM a")
    assert target_rows(raw) == [(1,)]


@pytest.mark.parametrize("cross_scope", [False, True])
@pytest.mark.parametrize("register_kill_first", [False, True])
def test_kill_precedes_remove_that_would_hide_the_kill_failure(
    placement_db, cross_scope, register_kill_first
):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1)")
    raw.execute("INSERT INTO b VALUES (-2)")
    remove_table = "b" if cross_scope else "a"
    policies = [
        ("a", "a.x > 0", Resolution.KILL),
        (remove_table, f"{remove_table}.x > 0", Resolution.REMOVE),
    ]
    if not register_kill_first:
        policies.reverse()
    for table, constraint, resolution in policies:
        add_policy(conn, table, constraint, resolution)
    query = (
        "WITH aa AS (SELECT x FROM a), bb AS (SELECT x FROM b) SELECT aa.x FROM aa CROSS JOIN bb"
        if cross_scope
        else "SELECT x FROM a"
    )
    with pytest.raises(Exception, match=KILL_MESSAGE) as caught:
        conn.execute(f"INSERT INTO target {query}")
    assert not isinstance(caught.value, RetryViolation)
    assert target_rows(raw) == []


@pytest.mark.parametrize("register_retry_first", [False, True])
@pytest.mark.parametrize("surviving_failure", [False, True])
def test_remove_filters_then_retry_checks_survivors(
    placement_db, register_retry_first, surviving_failure
):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1), (1)")
    if surviving_failure:
        raw.execute("INSERT INTO a VALUES (-2)")
    policies = [
        ("a", "a.x <> -1", Resolution.REMOVE, None),
        ("a", "a.x > 0", Resolution.RETRY, "Surviving negative value"),
    ]
    if register_retry_first:
        policies.reverse()
    for table, constraint, resolution, message in policies:
        add_policy(conn, table, constraint, resolution, message)
    if surviving_failure:
        with pytest.raises(RetryViolation) as caught:
            conn.execute("INSERT INTO target SELECT x FROM a")
        retry_id = 0 if register_retry_first else 1
        assert_failures(caught.value, [(retry_id, "Surviving negative value")])
        assert target_rows(raw) == []
    else:
        conn.execute("INSERT INTO target SELECT x FROM a")
        assert target_rows(raw) == [(1,)]


@pytest.mark.parametrize("surviving_failure", [False, True])
def test_cross_scope_remove_filters_before_retry(placement_db, surviving_failure):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1)")
    raw.execute("INSERT INTO b VALUES (-2)")
    if surviving_failure:
        raw.execute("INSERT INTO a VALUES (1)")
    add_policy(conn, "b", "b.x > 0", message="B negative on surviving result")
    add_policy(conn, "a", "a.x > 0", Resolution.REMOVE)
    sql = (
        "INSERT INTO target WITH aa AS (SELECT x FROM a), bb AS (SELECT x FROM b) "
        "SELECT aa.x FROM aa CROSS JOIN bb"
    )
    if surviving_failure:
        with pytest.raises(RetryViolation) as caught:
            conn.execute(sql)
        assert_failures(caught.value, [(0, "B negative on surviving result")])
    else:
        conn.execute(sql)
    assert target_rows(raw) == []


@pytest.mark.parametrize("splittable", [False, True])
@pytest.mark.parametrize("relation_udf", [False, True])
@pytest.mark.parametrize("cross_scope", [False, True])
def test_retry_precedes_lower_priority_udf_without_invoking_it(
    placement_db, splittable, relation_udf, cross_scope
):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1)")
    raw.execute("INSERT INTO b VALUES (-2)")
    effects = []

    def repair(value):
        effects.append(value)
        return True if relation_udf else value

    if relation_udf:
        conn.register_relation_resolution_function("record_repair", repair, splittable=splittable)
        resolution = Resolution.RELATION_UDF
    else:
        conn.register_resolution_function(
            "record_repair", repair, ["BIGINT"], "BIGINT", splittable=splittable
        )
        resolution = Resolution.UDF
    udf_table = "b" if cross_scope else "a"
    # Register lower-priority UDF first, so registration order cannot hide the bug.
    add_policy(conn, udf_table, f"{udf_table}.x > 0", resolution, udf="record_repair")
    add_policy(conn, "a", "a.x > 0", message="Retry before any repair effect")
    query = (
        "WITH aa AS (SELECT x FROM a), bb AS (SELECT x FROM b) SELECT aa.x FROM aa CROSS JOIN bb"
        if cross_scope
        else "SELECT x FROM a"
    )
    with pytest.raises(RetryViolation) as caught:
        conn.execute(f"INSERT INTO target {query}")
    assert_failures(caught.value, [(1, "Retry before any repair effect")])
    assert effects == []
    assert target_rows(raw) == []


@pytest.mark.parametrize("splittable", [False, True])
@pytest.mark.parametrize("relation_udf", [False, True])
def test_lower_priority_udf_is_reachable_after_retry_passes(placement_db, splittable, relation_udf):
    """Control for the no-effect test: the registered callback must be usable."""
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1)")
    effects = []

    def repair(value):
        effects.append(value)
        return True if relation_udf else abs(value)

    if relation_udf:
        conn.register_relation_resolution_function(
            "repair_after_retry", repair, splittable=splittable
        )
        resolution = Resolution.RELATION_UDF
    else:
        conn.register_resolution_function(
            "repair_after_retry", repair, ["BIGINT"], "BIGINT", splittable=splittable
        )
        resolution = Resolution.UDF
    add_policy(conn, "a", "a.x > 0", resolution, udf="repair_after_retry")
    add_policy(conn, "a", "a.x <> 0", message="Only zero should retry")
    conn.execute("INSERT INTO target SELECT x FROM a")
    # Count resolution effects, not arbitrary policy/projection expressions.
    assert effects == [True if relation_udf else -1]
    assert target_rows(raw) == [(-1 if relation_udf else 1,)]


@pytest.mark.parametrize("splittable", [False, True])
@pytest.mark.parametrize("relation_udf", [False, True])
def test_remove_all_rows_prevents_lower_resolution_callbacks(
    placement_db, splittable, relation_udf
):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1)")
    effects = []

    def repair(value):
        effects.append(value)
        return True if relation_udf else value

    if relation_udf:
        conn.register_relation_resolution_function("never_repair", repair, splittable=splittable)
        resolution = Resolution.RELATION_UDF
    else:
        conn.register_resolution_function(
            "never_repair", repair, ["BIGINT"], "BIGINT", splittable=splittable
        )
        resolution = Resolution.UDF
    add_policy(conn, "a", "a.x > 0", resolution, udf="never_repair")
    add_policy(conn, "a", "a.x > 0", message="Removed row must not retry")
    add_policy(conn, "a", "a.x > 0", Resolution.REMOVE)
    conn.execute("INSERT INTO target SELECT x FROM a")
    assert effects == []
    assert target_rows(raw) == []


def test_quoted_output_alias_survives_remove_and_retry_stages(placement_db):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1), (1)")
    add_policy(conn, "a", "a.x <> 0", message="No zero")
    add_policy(conn, "a", "a.x > 0", Resolution.REMOVE)
    conn.execute('INSERT INTO target SELECT x AS "out value" FROM a')
    assert target_rows(raw) == [(1,)]


@pytest.mark.parametrize("splittable", [False, True])
@pytest.mark.parametrize("relation_udf", [False, True])
def test_remove_and_udf_without_abort_resolution_handles_multiple_survivors(
    placement_db, splittable, relation_udf
):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-3), (-2), (-1), (9)")
    effects = []

    def repair(value):
        effects.append(value)
        return True if relation_udf else abs(value)

    if relation_udf:
        conn.register_relation_resolution_function("multi_repair", repair, splittable=splittable)
        resolution = Resolution.RELATION_UDF
    else:
        conn.register_resolution_function(
            "multi_repair", repair, ["BIGINT"], "BIGINT", splittable=splittable
        )
        resolution = Resolution.UDF
    add_policy(conn, "a", "a.x > 0", resolution, udf="multi_repair")
    add_policy(conn, "a", "a.x <> -3", Resolution.REMOVE)
    conn.execute("INSERT INTO target SELECT x FROM a")
    if relation_udf:
        assert effects == [True]
        assert target_rows(raw) == [(-2,), (-1,), (9,)]
    else:
        assert sorted(effects) == [-2, -1]
        assert target_rows(raw) == [(1,), (2,), (9,)]


def test_kill_takes_precedence_over_retry_even_when_retry_registered_first(placement_db):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1)")
    add_policy(conn, "a", "a.x > 0", message="Must not be returned before KILL")
    add_policy(conn, "a", "a.x > 0", Resolution.KILL)
    with pytest.raises(Exception, match=KILL_MESSAGE) as caught:
        conn.execute("INSERT INTO target SELECT x FROM a")
    assert not isinstance(caught.value, RetryViolation)
    assert target_rows(raw) == []


def test_mixed_resolution_stages_preserve_group_by_projection_alias(placement_db):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (1), (1), (2)")
    add_policy(conn, "a", "MAX(a.x) > 0", message="Positive groups only")
    add_policy(conn, "a", "MAX(a.x) <> 1", Resolution.REMOVE)
    conn.execute("INSERT INTO target SELECT x AS k FROM a GROUP BY k")
    assert target_rows(raw) == [(2,)]


def test_hidden_source_attribute_survives_transparent_cte_projection(placement_db):
    raw, conn = placement_db
    raw.execute("ALTER TABLE a ADD COLUMN allowed BOOLEAN")
    raw.execute("INSERT INTO a VALUES (1, false)")
    add_policy(conn, "a", "a.allowed", message="Hidden source flag is false")
    with pytest.raises(RetryViolation) as caught:
        conn.execute("INSERT INTO target WITH aa AS (SELECT x FROM a) SELECT x FROM aa")
    assert_failures(caught.value, [(0, "Hidden source flag is false")])
    assert target_rows(raw) == []


@pytest.mark.parametrize("outer_filter", ["", "WHERE k > 0"])
def test_cte_explicit_column_rename_preserves_policy_lineage(placement_db, outer_filter):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1), (1)")
    add_policy(conn, "a", "a.x > 0", message="Original source must be positive")
    sql = f"INSERT INTO target WITH aa(k) AS (SELECT x FROM a) SELECT k FROM aa {outer_filter}"
    if outer_filter:
        conn.execute(sql)
        assert target_rows(raw) == [(1,)]
    else:
        with pytest.raises(RetryViolation) as caught:
            conn.execute(sql)
        assert_failures(caught.value, [(0, "Original source must be positive")])
        assert target_rows(raw) == []


@pytest.mark.parametrize("outer_filter", ["", "WHERE x < 0"])
def test_computed_cte_projection_is_not_substituted_for_original_source_value(
    placement_db, outer_filter
):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1), (1)")
    add_policy(conn, "a", "a.x > 0", message="Check source, not negated output")
    sql = f"INSERT INTO target WITH aa AS (SELECT -x AS x FROM a) SELECT x FROM aa {outer_filter}"
    if outer_filter:
        # Output -1 derives from source +1; the source -1 row was eliminated.
        conn.execute(sql)
        assert target_rows(raw) == [(-1,)]
    else:
        with pytest.raises(RetryViolation) as caught:
            conn.execute(sql)
        assert_failures(caught.value, [(0, "Check source, not negated output")])
        assert target_rows(raw) == []


def test_quoted_hidden_attribute_and_cte_alias_keep_source_identity(placement_db):
    raw, conn = placement_db
    raw.execute('ALTER TABLE a ADD COLUMN "policy flag" BOOLEAN')
    raw.execute("INSERT INTO a VALUES (1, false)")
    add_policy(conn, "a", 'a."policy flag"', message="Quoted source flag failed")
    with pytest.raises(RetryViolation) as caught:
        conn.execute(
            'INSERT INTO target WITH "cte with space" AS (SELECT x FROM a) '
            'SELECT x FROM "cte with space"'
        )
    assert_failures(caught.value, [(0, "Quoted source flag failed")])
    assert target_rows(raw) == []


@pytest.mark.parametrize("allowed", [False, True])
def test_cte_hidden_policy_metadata_does_not_leak_through_star(placement_db, allowed):
    raw, conn = placement_db
    raw.execute("ALTER TABLE a ADD COLUMN hidden BOOLEAN")
    raw.execute("INSERT INTO a VALUES (7, ?)", [allowed])
    add_policy(conn, "a", "a.hidden", message="Hidden policy flag failed")
    sql = "INSERT INTO target WITH aa AS (SELECT x FROM a) SELECT * FROM aa"
    if allowed:
        conn.execute(sql)
        assert target_rows(raw) == [(7,)]
    else:
        with pytest.raises(RetryViolation) as caught:
            conn.execute(sql)
        assert_failures(caught.value, [(0, "Hidden policy flag failed")])
        assert target_rows(raw) == []


@pytest.mark.parametrize(
    "projection, condition",
    [
        ("l.x", "l.x = 1 AND r.x = -1"),
        ("r.x", "l.x = -1 AND r.x = 1"),
    ],
)
def test_repeated_source_in_cte_fails_closed_until_occurrence_lineage_supported(
    placement_db, projection, condition
):
    """Staging restriction: do not silently substitute one self-join occurrence."""
    raw, conn = placement_db
    raw.execute("ALTER TABLE a ADD COLUMN allowed BOOLEAN")
    raw.execute("INSERT INTO a VALUES (-1, false), (1, true)")
    add_policy(conn, "a", "a.allowed", message="Both contributing occurrences matter")
    with pytest.raises(PassantRewriteError):
        conn.execute(
            f"INSERT INTO target WITH aa AS (SELECT {projection} FROM a l "
            f"CROSS JOIN a r WHERE {condition}) SELECT x FROM aa"
        )
    assert target_rows(raw) == []


def test_quoted_qualified_star_preserves_original_cte_schema(placement_db):
    raw, conn = placement_db
    raw.execute("ALTER TABLE a ADD COLUMN allowed BOOLEAN")
    raw.execute("INSERT INTO a VALUES (-1, false), (1, true)")
    add_policy(conn, "a", "a.allowed", message="Only surviving source must pass")
    conn.execute(
        'INSERT INTO target WITH "aa space" AS (SELECT x FROM a) '
        'SELECT "aa space".* FROM "aa space" WHERE x > 0'
    )
    assert target_rows(raw) == [(1,)]


def test_retry_for_unrelated_sink_does_not_reject_values_insert(placement_db):
    raw, conn = placement_db
    add_policy(conn, "a", "a.x > 0", message="Protect target, not a")
    conn.execute("INSERT INTO a VALUES (1)")
    assert raw.execute("SELECT x FROM a").fetchall() == [(1,)]
    assert target_rows(raw) == []


def test_kill_only_transparent_cte_matches_direct_final_candidate_filter(placement_db):
    """The final-candidate contract is independent of RETRY's presence."""
    raw, conn = placement_db
    raw.execute("ALTER TABLE a ADD COLUMN allowed BOOLEAN")
    raw.execute("INSERT INTO a VALUES (-1, false), (1, true)")
    add_policy(conn, "a", "a.allowed", Resolution.KILL)
    for query in (
        "SELECT x FROM a WHERE x > 0",
        "WITH aa AS (SELECT x FROM a) SELECT x FROM aa WHERE x > 0",
    ):
        conn.execute(f"INSERT INTO target {query}")
        assert target_rows(raw) == [(1,)]
        raw.execute("DELETE FROM target")


@pytest.mark.parametrize("required", [False, True])
def test_absent_source_values_applicability_distinguishes_required(placement_db, required):
    raw, conn = placement_db
    conn.register_policy(
        Policy(
            sources=["a"],
            required_sources=["a"] if required else [],
            # REQUIRED applies at its declared sink. Source-only REQUIRED
            # applicability is unchanged by this staging regression.
            sink="target" if required else None,
            constraint="a.x > 0",
            on_fail=Resolution.RETRY,
            retry_message="A source must pass",
        )
    )
    if required:
        # Relevant VALUES enforcement is not silently bypassed. A supported
        # native rejection or explicit staging diagnostic is acceptable here.
        with pytest.raises((PassantRewriteError, RetryViolation)):
            conn.execute("INSERT INTO target VALUES (1)")
        assert target_rows(raw) == []
    else:
        conn.execute("INSERT INTO target VALUES (1)")
        assert target_rows(raw) == [(1,)]


@pytest.mark.parametrize("join", ["NATURAL JOIN b", "JOIN b USING (x)"])
def test_join_column_merging_fails_closed_before_hidden_metadata_changes_semantics(
    placement_db, join
):
    """Explicit prototype guard until join-aware hidden-column handling exists."""
    raw, conn = placement_db
    raw.execute("ALTER TABLE a ADD COLUMN allowed BOOLEAN")
    raw.execute("INSERT INTO a VALUES (-1, false), (1, true)")
    raw.execute("INSERT INTO b VALUES (1)")
    add_policy(conn, "a", "a.allowed", message="Hidden source flag")
    with pytest.raises(PassantRewriteError):
        conn.execute(f"INSERT INTO target WITH aa AS (SELECT x FROM a) SELECT x FROM aa {join}")
    assert target_rows(raw) == []


@pytest.mark.parametrize("resolution", [Resolution.RETRY, Resolution.KILL])
@pytest.mark.parametrize("transparent_cte", [False, True])
@pytest.mark.parametrize("keep_bad_source", [False, True])
def test_unrelated_repeated_table_does_not_block_protected_source_lineage(
    placement_db, resolution, transparent_cte, keep_bad_source
):
    """Demanded lineage tracks a, not the two unrelated occurrences of b."""
    raw, conn = placement_db
    raw.execute("ALTER TABLE a ADD COLUMN allowed BOOLEAN")
    raw.execute("INSERT INTO a VALUES (-1, false), (1, true)")
    raw.execute("INSERT INTO b VALUES (1), (1), (2)")
    add_policy(conn, "a", "a.allowed", resolution, message="Protected A failed")
    base = "SELECT a.x FROM a JOIN b left_b ON left_b.x = 1 JOIN b right_b ON right_b.x = 1"
    predicate = "x < 0" if keep_bad_source else "x > 0"
    query = (
        f"WITH aa AS ({base}) SELECT x FROM aa WHERE {predicate}"
        if transparent_cte
        else f"{base} WHERE a.{predicate}"
    )
    if keep_bad_source:
        error = RetryViolation if resolution is Resolution.RETRY else Exception
        message = "Protected A failed" if resolution is Resolution.RETRY else KILL_MESSAGE
        with pytest.raises(error, match=message):
            conn.execute(f"INSERT INTO target {query}")
        assert target_rows(raw) == []
    else:
        conn.execute(f"INSERT INTO target {query}")
        # Both b aliases have two matches. Neither hidden lineage nor repeated
        # table handling may collapse these four visible result records.
        assert target_rows(raw) == [(1,), (1,), (1,), (1,)]


@pytest.mark.parametrize(
    "query",
    [
        "SELECT d.x FROM (SELECT x FROM a) d JOIN b ON EXISTS (SELECT x FROM b)",
        "SELECT d.x FROM (SELECT x FROM a) d ORDER BY (SELECT max(x) FROM b)",
    ],
)
def test_expression_subquery_outside_select_header_fails_closed(placement_db, query):
    """Capability analysis must reject expression subqueries in every clause."""
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1)")
    raw.execute("INSERT INTO b VALUES (1)")
    add_policy(conn, "a", "a.x > 0", message="Do not bypass A policy")
    with pytest.raises(PassantRewriteError, match="(?i)expression subquer"):
        conn.execute(f"INSERT INTO target {query}")
    assert target_rows(raw) == []


def test_union_all_unprotected_arm_is_not_subject_to_other_source_policy(placement_db):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (1)")
    raw.execute("INSERT INTO b VALUES (-2)")
    add_policy(conn, "a", "a.x > 0", message="Only A must be positive")
    conn.execute("INSERT INTO target SELECT x FROM a UNION ALL SELECT x FROM b")
    assert target_rows(raw) == [(-2,), (1,)]


@pytest.mark.parametrize("allowed", [None, True])
def test_union_all_distinguishes_real_null_from_absent_source_padding(placement_db, allowed):
    raw, conn = placement_db
    raw.execute("ALTER TABLE a ADD COLUMN allowed BOOLEAN")
    raw.execute("INSERT INTO a VALUES (1, ?)", [allowed])
    raw.execute("INSERT INTO b VALUES (-2)")
    add_policy(conn, "a", "a.allowed", message="Actual A flag must be true")
    sql = "INSERT INTO target SELECT x FROM a UNION ALL SELECT x FROM b"
    if allowed is None:
        with pytest.raises(RetryViolation) as caught:
            conn.execute(sql)
        assert_failures(caught.value, [(0, "Actual A flag must be true")])
        assert target_rows(raw) == []
    else:
        # B has no A source; a NULL in A's padded metadata is not a violation.
        conn.execute(sql)
        assert target_rows(raw) == [(-2,), (1,)]


def test_union_all_outer_where_eliminates_violating_arm_before_retry(placement_db):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1)")
    raw.execute("INSERT INTO b VALUES (2)")
    add_policy(conn, "a", "a.x > 0", message="Eliminated A must not fail")
    conn.execute(
        "INSERT INTO target SELECT x FROM "
        "(SELECT x FROM a UNION ALL SELECT x FROM b) candidates WHERE x > 0"
    )
    assert target_rows(raw) == [(2,)]


@pytest.mark.parametrize(
    "query, expected",
    [
        ("(SELECT x FROM a ORDER BY x DESC LIMIT 1) UNION ALL SELECT x FROM b", [(1,), (2,)]),
        ("SELECT x FROM a UNION ALL SELECT x FROM b ORDER BY x DESC LIMIT 1", [(2,)]),
        ("SELECT x FROM a UNION ALL SELECT x FROM b ORDER BY x LIMIT 1", None),
    ],
)
def test_union_all_branch_and_outer_limits_define_actual_policy_candidates(
    placement_db, query, expected
):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1), (1)")
    raw.execute("INSERT INTO b VALUES (2)")
    add_policy(conn, "a", "a.x > 0", message="Retained negative A")
    if expected is None:
        with pytest.raises(RetryViolation) as caught:
            conn.execute(f"INSERT INTO target {query}")
        assert_failures(caught.value, [(0, "Retained negative A")])
        assert target_rows(raw) == []
    else:
        conn.execute(f"INSERT INTO target {query}")
        assert target_rows(raw) == expected


@pytest.mark.parametrize(
    "query",
    [
        "SELECT x FROM a UNION ALL SELECT 3 UNION ALL SELECT x FROM b",
        "(SELECT x FROM a UNION ALL SELECT 3) UNION ALL SELECT x FROM b",
        "SELECT x FROM a UNION ALL (SELECT 3 UNION ALL SELECT x FROM b)",
    ],
)
def test_nested_and_chained_union_all_preserves_source_applicability(placement_db, query):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1)")
    raw.execute("INSERT INTO b VALUES (-2)")
    add_policy(conn, "b", "b.x > 0", message="B failure first by ID")
    add_policy(conn, "a", "a.x > 0", message="A failure second by ID")
    with pytest.raises(RetryViolation) as caught:
        conn.execute(f"INSERT INTO target {query}")
    assert_failures(caught.value, [(0, "B failure first by ID"), (1, "A failure second by ID")])
    assert target_rows(raw) == []


def test_union_all_kill_is_not_hidden_by_remove(placement_db):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1)")
    raw.execute("INSERT INTO b VALUES (-2)")
    # Both arms would disappear under REMOVE, but KILL must run first.
    add_policy(conn, "b", "b.x > 0", Resolution.REMOVE)
    add_policy(conn, "a", "a.x > 0", Resolution.REMOVE)
    add_policy(conn, "a", "a.x > 0", Resolution.KILL)
    with pytest.raises(Exception, match=KILL_MESSAGE) as caught:
        conn.execute("INSERT INTO target SELECT x FROM a UNION ALL SELECT x FROM b")
    assert not isinstance(caught.value, RetryViolation)
    assert target_rows(raw) == []


def test_union_all_remove_one_arm_does_not_hide_retry_in_surviving_other_arm(placement_db):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1)")
    raw.execute("INSERT INTO b VALUES (-2)")
    add_policy(conn, "b", "b.x > 0", message="B still fails after removing A")
    add_policy(conn, "a", "a.x > 0", message="Removed A must not report")
    add_policy(conn, "a", "a.x > 0", Resolution.REMOVE)
    with pytest.raises(RetryViolation) as caught:
        conn.execute("INSERT INTO target SELECT x FROM a UNION ALL SELECT x FROM b")
    assert_failures(caught.value, [(0, "B still fails after removing A")])
    assert target_rows(raw) == []


@pytest.mark.parametrize("declared_sink", [None, "target"])
def test_union_all_required_source_preserves_existing_sink_scoped_applicability(
    placement_db, declared_sink
):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (1)")
    raw.execute("INSERT INTO b VALUES (-2)")
    conn.register_policy(
        Policy(
            sources=["a"],
            required_sources=["a"],
            sink=declared_sink,
            constraint="a.x > 0",
            on_fail=Resolution.RETRY,
            retry_message="Declared sink requires A provenance",
        )
    )
    sql = "INSERT INTO target SELECT x FROM a UNION ALL SELECT x FROM b"
    if declared_sink:
        with pytest.raises(RetryViolation) as caught:
            conn.execute(sql)
        assert_failures(caught.value, [(0, "Declared sink requires A provenance")])
        assert target_rows(raw) == []
    else:
        # Legacy no-sink REQUIRED is not a new global provenance requirement:
        # its ordinary source-presence applicability does not cover B's arm.
        conn.execute(sql)
        assert target_rows(raw) == [(-2,), (1,)]


def test_terminal_union_distinct_merges_source_provenance_before_aggregate_constraint(placement_db):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (1), (2)")
    raw.execute("INSERT INTO b VALUES (1), (3)")
    conn.register_policy(
        Policy(
            sources=["a", "b"],
            sink="target",
            constraint="MAX(a.x) > 1 AND MAX(b.x) > 1",
            on_fail=Resolution.RETRY,
            retry_message="Both merged source aggregates must pass",
        )
    )
    conn.execute("INSERT INTO target SELECT 7 AS x FROM a UNION SELECT 7 AS x FROM b")
    # One visible tuple retains both source contributions, not one chosen arm.
    assert target_rows(raw) == [(7,)]


@pytest.mark.parametrize("constraint", ["a.x > 1", "a.x > 0"])
def test_terminal_union_distinct_scalar_policy_rejects_nonunique_hidden_values(
    placement_db, constraint
):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (1), (2)")
    raw.execute("INSERT INTO b VALUES (3)")
    add_policy(conn, "a", constraint, message="Scalar source value must be unique")
    with pytest.raises(RetryViolation) as caught:
        conn.execute("INSERT INTO target SELECT 7 AS x FROM a UNION SELECT 7 AS x FROM b")
    assert_failures(caught.value, [(0, "Scalar source value must be unique")])
    assert target_rows(raw) == []


def test_terminal_union_distinct_repeated_identical_hidden_source_value_is_unique(placement_db):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (1), (1)")
    raw.execute("INSERT INTO b VALUES (-3)")
    add_policy(conn, "a", "a.x = 1", message="A must uniquely equal one")
    conn.execute("INSERT INTO target SELECT 7 AS x FROM a UNION SELECT 7 AS x FROM b")
    assert target_rows(raw) == [(7,)]


@pytest.mark.parametrize("present_null", [False, True])
def test_terminal_union_distinct_distinguishes_present_null_from_absent_source_padding(
    placement_db, present_null
):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (1)")
    if present_null:
        raw.execute("INSERT INTO a VALUES (NULL)")
    raw.execute("INSERT INTO b VALUES (2)")
    add_policy(conn, "a", "a.x = 1", message="A must have one non-NULL scalar value")
    sql = "INSERT INTO target SELECT 7 AS x FROM a UNION SELECT 7 AS x FROM b"
    if present_null:
        with pytest.raises(RetryViolation) as caught:
            conn.execute(sql)
        assert_failures(caught.value, [(0, "A must have one non-NULL scalar value")])
        assert target_rows(raw) == []
    else:
        # B has no A contribution; its padded NULL is not an actual source NULL.
        conn.execute(sql)
        assert target_rows(raw) == [(7,)]


def test_terminal_union_distinct_outer_limit_eliminates_bad_deduplicated_tuple(placement_db):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (-1), (1)")
    raw.execute("INSERT INTO b VALUES (1)")
    add_policy(conn, "a", "a.x > 0", message="Eliminated deduplicated tuple must not fail")
    conn.execute("INSERT INTO target SELECT x FROM a UNION SELECT x FROM b ORDER BY x DESC LIMIT 1")
    assert target_rows(raw) == [(1,)]


def test_terminal_union_distinct_returns_each_actual_scalar_uniqueness_failure(placement_db):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (1), (2)")
    raw.execute("INSERT INTO b VALUES (1), (3)")
    add_policy(conn, "b", "b.x > 0", message=" B's distinct failure ☃ ")
    add_policy(conn, "a", "a.x > 0", message="A's distinct failure")
    with pytest.raises(RetryViolation) as caught:
        conn.execute("INSERT INTO target SELECT 7 AS x FROM a UNION SELECT 7 AS x FROM b")
    assert_failures(caught.value, [(0, " B's distinct failure ☃ "), (1, "A's distinct failure")])
    assert target_rows(raw) == []


@pytest.mark.parametrize("all_positive", [False, True])
@pytest.mark.parametrize("target_reference", ["target2", "_OUTPUT_"])
@pytest.mark.parametrize("operator", ["UNION ALL", "UNION"])
def test_union_sink_policy_uses_explicit_insert_target_column_order(
    placement_db, all_positive, target_reference, operator
):
    raw, conn = placement_db
    raw.execute("CREATE TABLE target2 (left_value INTEGER, right_value INTEGER)")
    conn.register_policy(
        Policy(
            sources=[],
            sink="target2",
            constraint=f"{target_reference}.left_value > 0",
            on_fail=Resolution.RETRY,
            retry_message="Left target column must be positive",
        )
    )
    second_left = 2 if all_positive else -2
    sql = (
        "INSERT INTO target2 (right_value, left_value) "
        "SELECT 10 AS positive_first, 1 AS second_value "
        f"{operator} SELECT 20, {second_left}"
    )
    if all_positive:
        conn.execute(sql)
        assert raw.execute(
            "SELECT left_value, right_value FROM target2 ORDER BY left_value"
        ).fetchall() == [
            (1, 10),
            (2, 20),
        ]
    else:
        # Positive first projection values are right_value, not left_value.
        with pytest.raises(RetryViolation) as caught:
            conn.execute(sql)
        assert_failures(caught.value, [(0, "Left target column must be positive")])
        assert raw.execute("SELECT * FROM target2").fetchall() == []


@pytest.mark.parametrize(
    "values, constraint, passes",
    [
        ((1, None, 2), "COUNT(a.x) = 2", True),
        ((1, None, 2), "COUNT(*) = 3", True),
        ((1, None, 2), "COUNT(DISTINCT a.x) = 2", True),
        ((1, None, 2), "SUM(a.x) = 3", True),
        ((1, None, 2), "AVG(a.x) = 1.5", True),
        ((1, None, 2), "COUNT(*) = 5", False),
        ((1, 1, None), "SUM(a.x) = 2 AND COUNT(a.x) = 2 AND COUNT(*) = 3", True),
        ((None,), "COUNT(*) = 1 AND COUNT(a.x) = 0", True),
    ],
)
def test_terminal_union_distinct_aggregates_preserve_present_source_multiset(
    placement_db, values, constraint, passes
):
    raw, conn = placement_db
    raw.executemany("INSERT INTO a VALUES (?)", [(value,) for value in values])
    raw.execute("INSERT INTO b VALUES (9), (9)")
    add_policy(conn, "a", constraint, message="Check only actual A contributions")
    sql = "INSERT INTO target SELECT 7 AS x FROM a UNION SELECT 7 AS x FROM b"
    if passes:
        conn.execute(sql)
        assert target_rows(raw) == [(7,)]
    else:
        with pytest.raises(RetryViolation) as caught:
            conn.execute(sql)
        assert_failures(caught.value, [(0, "Check only actual A contributions")])
        assert target_rows(raw) == []


@pytest.mark.parametrize("transparent_cte", [False, True])
@pytest.mark.parametrize(
    "hidden_values, constraint, passes",
    [
        ((1, 2), "a.y > 0", False),
        ((1, 1), "a.y > 0", True),
        ((1, 2), "COUNT(a.y) = 2 AND AVG(a.y) = 1.5", True),
    ],
)
def test_root_select_distinct_preserves_raw_contributors_before_policy_evaluation(
    placement_db, transparent_cte, hidden_values, constraint, passes
):
    raw, conn = placement_db
    raw.execute("ALTER TABLE a ADD COLUMN y INTEGER")
    raw.executemany("INSERT INTO a VALUES (7, ?)", [(value,) for value in hidden_values])
    add_policy(conn, "a", constraint, message="Evaluate all contributors of the distinct row")
    query = (
        "WITH aa AS (SELECT x FROM a) SELECT DISTINCT x FROM aa"
        if transparent_cte
        else "SELECT DISTINCT x FROM a"
    )
    if passes:
        conn.execute(f"INSERT INTO target {query}")
        cursor = raw.execute("SELECT * FROM target")
        assert cursor.fetchall() == [(7,)]
        assert [column[0] for column in cursor.description] == ["x"]
    else:
        with pytest.raises(RetryViolation) as caught:
            conn.execute(f"INSERT INTO target {query}")
        assert_failures(caught.value, [(0, "Evaluate all contributors of the distinct row")])
        assert target_rows(raw) == []


@pytest.mark.parametrize(
    "query",
    [
        'WITH aa AS (SELECT x + 1 FROM a) SELECT "(x + 1)" FROM aa',
        'SELECT d."(x + 1)" FROM (SELECT x + 1 FROM a) d',
    ],
)
def test_insert_child_computed_label_remains_visible_to_parent_sql(placement_db, query):
    """INSERT root ordinals do not make referenced child-query labels private."""
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (1), (2)")
    add_policy(conn, "a", "a.x > 0", message="Positive source values")
    conn.execute(f"INSERT INTO target {query}")
    assert target_rows(raw) == [(2,), (3,)]


@pytest.mark.parametrize(
    "query, expected",
    [
        ('SELECT x + 1 FROM a ORDER BY "(x + 1)" DESC LIMIT 1', 3),
        ("SELECT x + 1 FROM a ORDER BY 1 DESC LIMIT 1", 3),
        ('SELECT x + 1 AS "value" FROM a ORDER BY "value" DESC LIMIT 1', 3),
        ("SELECT -x FROM a ORDER BY x DESC LIMIT 1", -2),
    ],
)
def test_insert_root_label_references_and_source_order_are_preserved(placement_db, query, expected):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (1), (2)")
    add_policy(conn, "a", "a.x > 0", message="Positive source values")
    conn.execute(f"INSERT INTO target {query}")
    # The target's column name x must not shadow source x in the final case:
    # ORDER BY source x DESC picks source 2, whose projected value is -2.
    assert target_rows(raw) == [(expected,)]


@pytest.mark.parametrize("constraint", ["a.x > 0 OR TRUE", "NOT (a.x < 0)"])
def test_scalar_uniqueness_is_a_precondition_on_the_entire_policy(placement_db, constraint):
    raw, conn = placement_db
    raw.execute("INSERT INTO a VALUES (1), (2)")
    add_policy(conn, "a", constraint, message="Whole-policy uniqueness required")
    with pytest.raises(RetryViolation) as caught:
        conn.execute("INSERT INTO target SELECT DISTINCT 7 AS x FROM a")
    assert_failures(caught.value, [(0, "Whole-policy uniqueness required")])
    assert target_rows(raw) == []
