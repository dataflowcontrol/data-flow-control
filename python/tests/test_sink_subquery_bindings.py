"""Sink references in Boolean policy subqueries retain their lexical bindings."""

import duckdb
import pytest
from data_flow_control import PassantRewriteError, Policy, RetryViolation, dfc


def _connection(constraint, *, dimension=False):
    raw = duckdb.connect(config={"lambda_syntax": "ENABLE_SINGLE_ARROW"})
    raw.execute("CREATE TABLE target(x INTEGER)")
    raw.execute("CREATE TABLE lookup(x INTEGER); INSERT INTO lookup VALUES (9)")
    conn = dfc(raw)
    dimension_clause = "DIMENSION lookup AS d " if dimension else ""
    conn.register_policy(
        Policy.from_pgn(
            f"SINK target AS i {dimension_clause}CONSTRAINT {constraint} "
            "ON FAIL RETRY('predicate rejected')"
        )
    )
    return raw, conn


@pytest.mark.parametrize(
    "constraint",
    [
        "EXISTS (SELECT 1 WHERE i.x = 1)",
        "NOT EXISTS (SELECT 1 WHERE i.x <> 1)",
        "(SELECT i.x) = 1",
        "1 IN (SELECT i.x)",
        "EXISTS (SELECT 1 WHERE EXISTS (SELECT 1 WHERE i.x = 1))",
    ],
)
def test_sink_reference_inside_subquery_allows_and_retries(constraint):
    raw, conn = _connection(constraint)
    with conn:
        conn.execute("INSERT INTO target SELECT 1")
        with pytest.raises(RetryViolation, match="predicate rejected"):
            conn.execute("INSERT INTO target SELECT 2")
        assert raw.execute("SELECT * FROM target").fetchall() == [(1,)]


@pytest.mark.parametrize(
    "constraint",
    [
        "i.x = 1 AND EXISTS (SELECT 1 FROM lookup AS i WHERE i.x = 9)",
        # The first branch's local i must not shadow the sink in its sibling.
        "EXISTS (SELECT 1 FROM lookup AS i WHERE FALSE UNION ALL SELECT 1 WHERE i.x = 1)",
        # A main-query alias is not visible inside the preceding CTE definition.
        (
            "EXISTS (WITH q AS (SELECT i.x AS x) SELECT 1 FROM lookup AS i "
            "JOIN q ON q.x = 1 WHERE i.x = 9)"
        ),
        # The derived table's own alias is not yet bound inside its definition.
        "EXISTS (SELECT 1 FROM (SELECT i.x AS x) AS i WHERE i.x = 1)",
        # The UNNEST alias shadows the sink only after evaluating its argument.
        "EXISTS (SELECT 1 FROM unnest([i.x]) AS i(x) WHERE i.x = 1)",
    ],
)
def test_inner_aliases_do_not_capture_sink_references(constraint):
    raw, conn = _connection(constraint, dimension=True)
    with conn:
        conn.execute("INSERT INTO target SELECT 1")
        with pytest.raises(RetryViolation, match="predicate rejected"):
            conn.execute("INSERT INTO target SELECT 2")
        assert raw.execute("SELECT * FROM target").fetchall() == [(1,)]


@pytest.mark.parametrize(
    "cc,bcc,rejected",
    [
        (None, None, False),
        ("  ALLOWED@example.test  ", "", False),
        ("outsider@example.test", None, True),
        (None, "outsider@example.test", True),
    ],
)
def test_not_exists_recipient_counterexample_with_quoted_sink_field(cc, bcc, rejected):
    raw = duckdb.connect(config={"lambda_syntax": "ENABLE_SINGLE_ARROW"})
    raw.execute('CREATE TABLE target("to" VARCHAR, cc VARCHAR, bcc VARCHAR)')
    raw.execute("CREATE TABLE recipients(email VARCHAR)")
    raw.execute("INSERT INTO recipients VALUES ('allowed@example.test')")
    policy = Policy.from_pgn(
        'SINK target AS i DIMENSION recipients AS d CONSTRAINT NOT EXISTS ('
        "SELECT 1 FROM unnest(string_split(lower(coalesce(i.\"to\", '') || ',' || "
        "coalesce(i.cc, '') || ',' || coalesce(i.bcc, '')), ',')) AS r(address) "
        "WHERE trim(r.address) <> '' AND NOT EXISTS "
        "(SELECT 1 FROM recipients AS a WHERE a.email = trim(r.address))) "
        "ON FAIL RETRY('unauthorized recipient')"
    )
    with dfc(raw) as conn:
        conn.register_policy(policy)
        def literal(value):
            return "NULL" if value is None else "'" + value.replace("'", "''") + "'"

        sql = (
            'INSERT INTO target ("to", cc, bcc) SELECT '
            f"'allowed@example.test', {literal(cc)}, {literal(bcc)}"
        )
        if rejected:
            with pytest.raises(RetryViolation, match="unauthorized recipient"):
                conn.execute(sql)
            assert raw.execute("SELECT * FROM target").fetchall() == []
        else:
            conn.execute(sql)
            assert raw.execute("SELECT * FROM target").fetchall() == [
                ("allowed@example.test", cc, bcc)
            ]


@pytest.mark.parametrize("projection", ["x", "s.x"])
def test_nested_policy_scope_rejects_potential_projection_capture(projection):
    raw, conn = _connection(
        "EXISTS (SELECT 1 FROM lookup AS s WHERE i.x = s.x)", dimension=True
    )
    with conn:
        # Replacing i.x with bare x or s.x would accidentally bind lookup's x=9,
        # allowing this x=1 input. Until alpha-renaming/materialization is available,
        # this shape must fail explicitly rather than silently weaken the policy.
        with pytest.raises(PassantRewriteError, match="capture"):
            conn.execute(f"INSERT INTO target SELECT {projection} FROM (SELECT 1 AS x) AS s")
        assert raw.execute("SELECT * FROM target").fetchall() == []


def test_qualified_outer_projection_is_preserved_inside_nested_lookup():
    raw, conn = _connection(
        "EXISTS (SELECT 1 FROM lookup AS l WHERE i.x = 1 AND l.x = 9)", dimension=True
    )
    with conn:
        conn.execute("INSERT INTO target SELECT s.x FROM (SELECT 1 AS x) AS s")
        with pytest.raises(RetryViolation, match="predicate rejected"):
            conn.execute("INSERT INTO target SELECT s.x FROM (SELECT 2 AS x) AS s")
        assert raw.execute("SELECT * FROM target").fetchall() == [(1,)]


def test_unaliased_table_function_qualifier_shadows_same_named_sink_alias():
    raw = duckdb.connect(config={"lambda_syntax": "ENABLE_SINGLE_ARROW"})
    raw.execute("CREATE TABLE target(unnest INTEGER)")
    with dfc(raw) as conn:
        conn.register_policy(
            Policy.from_pgn(
                "SINK target AS unnest CONSTRAINT unnest.unnest = 1 AND "
                "EXISTS (SELECT 1 FROM unnest([9]) WHERE unnest.unnest = 9) "
                "ON FAIL RETRY('predicate rejected')"
            )
        )
        conn.execute("INSERT INTO target SELECT 1")
        with pytest.raises(RetryViolation, match="predicate rejected"):
            conn.execute("INSERT INTO target SELECT 2")
        assert raw.execute("SELECT * FROM target").fetchall() == [(1,)]
