"""A shared repair function must negate the complete policy conjunction."""

import duckdb
import pytest
from data_flow_control import Policy, Resolution, dfc


@pytest.mark.parametrize("relation", [False, True])
@pytest.mark.parametrize("scope", ["same", "union"])
@pytest.mark.parametrize(
    "a_pass,b_pass",
    [
        (True, True),
        (False, True),
        (True, False),
        (False, False),
        (None, True),
        (True, None),
        (None, False),
        (False, None),
        (None, None),
    ],
)
def test_shared_udf_negates_all_policy_checks(relation, scope, a_pass, b_pass):
    raw = duckdb.connect()
    raw.execute("CREATE TABLE a(x INTEGER, allowed BOOLEAN, other BOOLEAN)")
    raw.execute("CREATE TABLE b(x INTEGER, allowed BOOLEAN)")
    raw.execute("CREATE TABLE target(x INTEGER)")
    raw.execute("INSERT INTO a VALUES (1, ?, ?)", [a_pass, b_pass])
    raw.execute("INSERT INTO b VALUES (2, ?)", [b_pass])
    conn = dfc(raw)
    effects = []

    def repair(value):
        effects.append(value)
        return True if relation else value + 10

    try:
        if relation:
            conn.register_relation_resolution_function("shared_repair", repair, splittable=False)
        else:
            conn.register_resolution_function(
                "shared_repair", repair, ["BIGINT"], "BIGINT", splittable=False
            )
        checks = [("a", "a.allowed"), ("a", "a.other")]
        if scope == "union":
            checks[1] = ("b", "b.allowed")
        for source, constraint in checks:
            conn.register_policy(
                Policy(
                    sources=[source],
                    sink="target",
                    constraint=constraint,
                    on_fail=Resolution.RELATION_UDF if relation else Resolution.UDF,
                    udf_name="shared_repair",
                )
            )
        query = "SELECT x FROM a"
        if scope == "union":
            query += " UNION ALL SELECT x FROM b"
        conn.execute("INSERT INTO target " + query)
        # UDF preserves SQL three-valued logic: an unknown check is not
        # repaired; tuple repair filters it, while an all-unknown relation aggregate skips the strict UDF.
        statuses = [a_pass, b_pass]
        if scope == "same":
            statuses = [False if False in statuses else None if None in statuses else True]
        failed = [index + 1 for index, status in enumerate(statuses) if status is False]
        expected = [(index + 1,) for index in range(len(statuses))]
        if not relation:
            expected = [
                (index + 1 + (10 if status is False else 0),)
                for index, status in enumerate(statuses)
                if status is not None
            ]
        all_unknown = all(status is None for status in statuses)
        if relation and all_unknown:
            expected = []
        assert raw.execute("SELECT x FROM target ORDER BY x").fetchall() == sorted(expected)
        expected_effects = [] if all_unknown else [bool(failed)]
        assert sorted(effects) == (expected_effects if relation else failed)
    finally:
        conn.close()
