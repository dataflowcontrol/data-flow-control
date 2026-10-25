"""Each independently registered policy must remain an AND operand."""

from itertools import product

import duckdb
import pytest

from data_flow_control import Policy, Resolution, dfc


@pytest.mark.parametrize(
    "constraints",
    [
        [],
        ["flags.a = TRUE OR flags.b = TRUE"],
        ["flags.a = TRUE OR flags.b = TRUE", "flags.c = TRUE"],
        ["flags.c = TRUE", "flags.a = TRUE OR flags.b = TRUE"],
        ["flags.a = TRUE OR flags.b = TRUE", "flags.c = TRUE OR flags.d = TRUE"],
        ["flags.a = TRUE AND flags.b = TRUE OR flags.c = TRUE", "flags.d = TRUE"],
        [
            "flags.a = TRUE OR (flags.b = TRUE AND flags.c = TRUE)",
            "flags.d = TRUE OR flags.a = TRUE",
            "flags.b = TRUE",
        ],
        [
            "(flags.a = TRUE OR flags.b = TRUE) AND (flags.c = TRUE OR flags.d = TRUE)",
            "flags.a = TRUE OR flags.d = TRUE",
        ],
    ],
)
@pytest.mark.parametrize("resolution", [Resolution.REMOVE, Resolution.KILL])
def test_policy_conjunction_matches_parenthesized_sql(constraints, resolution):
    # Exhaustive Boolean rows include both accepting and rejecting cases. KILL
    # evaluates one row at a time so a rejection cannot hide an over-rejection.
    raw = duckdb.connect()
    raw.execute("CREATE TABLE flags (a BOOLEAN, b BOOLEAN, c BOOLEAN, d BOOLEAN)")
    guarded = dfc(raw)
    guarded.register_policies(
        [Policy(sources=["flags"], constraint=c, on_fail=resolution) for c in constraints]
    )
    expected_sql = " AND ".join(f"({c})" for c in constraints) or "TRUE"
    try:
        for row in product([False, True], repeat=4):
            raw.execute("DELETE FROM flags")
            raw.execute("INSERT INTO flags VALUES (?, ?, ?, ?)", row)
            expected = raw.execute(f"SELECT a, b, c, d FROM flags WHERE {expected_sql}").fetchall()
            if resolution == Resolution.KILL and not expected:
                with pytest.raises(Exception, match="KILLing due to dfc policy violation"):
                    guarded.fetchall("SELECT a, b, c, d FROM flags")
            else:
                assert guarded.fetchall("SELECT a, b, c, d FROM flags") == expected
    finally:
        guarded.close()


@pytest.mark.parametrize("reverse", [False, True])
def test_kill_or_policy_cannot_bypass_second_policy_with_null(reverse):
    raw = duckdb.connect()
    raw.execute("CREATE TABLE action (tool VARCHAR, kind VARCHAR, text_value VARCHAR)")
    raw.execute("INSERT INTO action VALUES ('slack', 'string', NULL)")
    guarded = dfc(raw)
    constraints = [
        "action.kind <> 'recipient' OR action.text_value = 'ok'",
        "action.tool <> 'slack'",
    ]
    if reverse:
        constraints.reverse()
    guarded.register_policies(
        [Policy(sources=["action"], constraint=c, on_fail=Resolution.KILL) for c in constraints]
    )
    try:
        with pytest.raises(Exception, match="KILLing due to dfc policy violation"):
            guarded.fetchall("SELECT tool, kind, text_value FROM action")
    finally:
        guarded.close()


@pytest.mark.parametrize(
    "query,constraint,expected",
    [
        (
            "SELECT id FROM foo WHERE id = 1 OR id = 3",
            "max(foo.id) > 1",
            [(3,)],
        ),
        (
            "SELECT category, SUM(amount) FROM foo GROUP BY category "
            "HAVING SUM(amount) > 0 OR SUM(amount) < 0",
            "max(foo.amount) > 6",
            [("large", 10)],
        ),
    ],
)
def test_existing_or_clause_cannot_bypass_policy(query, constraint, expected):
    raw = duckdb.connect()
    raw.execute("CREATE TABLE foo (id INTEGER, category VARCHAR, amount INTEGER)")
    raw.execute("INSERT INTO foo VALUES (1, 'small', 1), (3, 'large', 10)")
    guarded = dfc(raw)
    guarded.register_policy(
        Policy(sources=["foo"], constraint=constraint, on_fail=Resolution.REMOVE)
    )
    try:
        assert guarded.fetchall(query) == expected
    finally:
        guarded.close()
