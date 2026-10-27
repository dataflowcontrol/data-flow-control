"""Every declared ordinary SOURCE must contribute; splitting cannot narrow scope."""

import duckdb
import pytest
from data_flow_control import Policy, Resolution, dfc
from data_flow_control.planner import Planner


@pytest.mark.parametrize(
    "catalog", [False, True], ids=["explicit-planner-catalog", "database-catalog"]
)
@pytest.mark.parametrize("constraint", ["MAX(foo.id) > 1 AND MAX(bar.id) > 1", "MAX(foo.id) > 1"])
def test_disjoint_union_all_keeps_policy_inapplicable(catalog, constraint):
    raw = duckdb.connect()
    raw.execute("CREATE TABLE foo(id INT); CREATE TABLE bar(id INT)")
    raw.execute("INSERT INTO foo VALUES (1),(2); INSERT INTO bar VALUES (1),(3)")
    policy = Policy(sources=["foo", "bar"], constraint=constraint, on_fail=Resolution.REMOVE)
    try:
        if catalog:
            connection = dfc(raw)
            connection.register_policy(policy)
            rewritten = connection.transform_query(
                "SELECT id FROM foo UNION ALL SELECT id FROM bar"
            )
        else:
            planner = Planner()
            planner.sync_catalog(
                {
                    "tables": {
                        name: {"columns": ["id"], "types": {"id": "INTEGER"}}
                        for name in ("foo", "bar")
                    }
                }
            )
            planner.register_policy(policy)
            rewritten = planner.rewrite("SELECT id FROM foo UNION ALL SELECT id FROM bar")
        assert sorted(raw.execute(rewritten).fetchall()) == [(1,), (1,), (2,), (3,)]
    finally:
        raw.close()


@pytest.mark.parametrize(
    "catalog", [False, True], ids=["explicit-planner-catalog", "database-catalog"]
)
def test_direct_join_with_every_source_still_enforces_policy(catalog):
    raw = duckdb.connect()
    raw.execute("CREATE TABLE foo(id INT); CREATE TABLE bar(id INT)")
    raw.execute("INSERT INTO foo VALUES (1),(2); INSERT INTO bar VALUES (1),(3)")
    policy = Policy(
        sources=["foo", "bar"],
        constraint="MAX(foo.id) > 1 AND MAX(bar.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    query = "SELECT foo.id AS f, bar.id AS b FROM foo CROSS JOIN bar"
    try:
        if catalog:
            connection = dfc(raw)
            connection.register_policy(policy)
            rewritten = connection.transform_query(query)
        else:
            planner = Planner()
            planner.sync_catalog(
                {
                    "tables": {
                        name: {"columns": ["id"], "types": {"id": "INTEGER"}}
                        for name in ("foo", "bar")
                    }
                }
            )
            planner.register_policy(policy)
            rewritten = planner.rewrite(query)
        assert raw.execute(rewritten).fetchall() == [(2, 3)]
    finally:
        raw.close()


@pytest.mark.parametrize("join", ["LEFT", "RIGHT", "FULL"])
@pytest.mark.parametrize("matched", [False, True])
def test_outer_scope_distinguishes_all_null_contributor_from_padding(join, matched):
    raw = duckdb.connect()
    raw.execute("CREATE TABLE a(v INT); CREATE TABLE b(v INT)")
    raw.execute("INSERT INTO a VALUES(NULL); INSERT INTO b VALUES(NULL)")
    query = f'SELECT a.v AS "left value", b.v AS "right value" FROM a {join} JOIN b ON {matched}'
    expected_schema = [(c[0], str(c[1])) for c in raw.execute(query).description]
    with dfc(raw) as conn:
        conn.register_policy(
            Policy(sources=["a", "b"], constraint="FALSE", on_fail=Resolution.REMOVE)
        )
        cursor = conn.execute(query)
        expected_rows = [] if matched else [(None, None)] * (2 if join == "FULL" else 1)
        assert cursor.fetchall() == expected_rows
        assert [(c[0], str(c[1])) for c in cursor.description] == expected_schema


@pytest.mark.parametrize("join", ["LEFT", "RIGHT", "FULL"])
@pytest.mark.parametrize("matched", [False, True])
def test_outer_retry_exact_failures_atomicity_and_inapplicable_padding(join, matched):
    from data_flow_control import RetryViolation

    raw = duckdb.connect()
    raw.execute("CREATE TABLE a(v INT); CREATE TABLE b(v INT); CREATE TABLE target(av INT,bv INT)")
    raw.execute("INSERT INTO a VALUES(NULL); INSERT INTO b VALUES(NULL)")
    query = f"INSERT INTO target SELECT a.v AS av,b.v AS bv FROM a {join} JOIN b ON {matched}"
    with dfc(raw) as conn:
        for constraint, message in [
            ("TRUE", "must not appear"),
            ("a.v > 0", "A's exact\nNULL ☃"),
            ("b.v > 0", "B actual NULL"),
        ]:
            conn.register_policy(
                Policy(
                    sources=["a", "b"],
                    sink="target",
                    constraint=constraint,
                    on_fail=Resolution.RETRY,
                    retry_message=message,
                )
            )
        if matched:
            with pytest.raises(RetryViolation) as caught:
                conn.execute(query)
            assert caught.value.messages == ["A's exact\nNULL ☃", "B actual NULL"]
            assert [failure.policy_index for failure in caught.value.failures] == [1, 2]
            assert raw.execute("SELECT * FROM target").fetchall() == []
        else:
            conn.execute(query)
            assert raw.execute("SELECT * FROM target").fetchall() == [(None, None)] * (
                2 if join == "FULL" else 1
            )


def test_outer_scope_preserves_multiplicity_quoted_aliases_and_hidden_name_collision():
    raw = duckdb.connect()
    raw.execute("CREATE TABLE a(k INT, __passant_lineage_0 INT); CREATE TABLE b(k INT)")
    raw.execute(
        "INSERT INTO a VALUES(1,10),(1,20),(2,30),(2,40); INSERT INTO b VALUES(1),(1),(1),(3),(3),(3)"
    )
    query = """SELECT "left side".k AS "Left Key", "right side".k AS "Right Key",
                      "left side".__passant_lineage_0 AS "Actual User Column"
               FROM a AS "left side" FULL JOIN b AS "right side"
               ON "left side".k="right side".k"""
    expected = raw.execute(query + ' WHERE "left side".k IS NULL OR "right side".k IS NULL')
    rows = expected.fetchall()
    schema = [(c[0], str(c[1])) for c in expected.description]
    with dfc(raw) as conn:
        conn.register_policy(
            Policy(sources=["a", "b"], constraint="FALSE", on_fail=Resolution.REMOVE)
        )
        cursor = conn.execute(query)
        assert sorted(cursor.fetchall(), key=str) == sorted(rows, key=str)
        assert len(rows) == 5
        assert [(c[0], str(c[1])) for c in cursor.description] == schema


@pytest.mark.parametrize("exclude_match", [False, True])
def test_outer_retry_atomicity_and_where_eliminated_match(exclude_match):
    from data_flow_control import RetryViolation

    raw = duckdb.connect()
    raw.execute("CREATE TABLE a(k INT); CREATE TABLE b(k INT); CREATE TABLE target(ak INT,bk INT)")
    raw.execute("INSERT INTO a VALUES(1),(2); INSERT INTO b VALUES(1),(3)")
    with dfc(raw) as conn:
        conn.register_policy(
            Policy(
                sources=["a", "b"],
                sink="target",
                constraint="FALSE",
                on_fail=Resolution.RETRY,
                retry_message="only matched rows",
            )
        )
        query = "INSERT INTO target SELECT a.k AS ak,b.k AS bk FROM a FULL JOIN b ON a.k=b.k"
        if exclude_match:
            conn.execute(query + " WHERE COALESCE(a.k,b.k)<>1")
            assert sorted(raw.execute("SELECT * FROM target").fetchall(), key=str) == sorted(
                [(2, None), (None, 3)], key=str
            )
        else:
            with pytest.raises(RetryViolation) as caught:
                conn.execute(query)
            assert caught.value.messages == ["only matched rows"]
            assert raw.execute("SELECT * FROM target").fetchall() == []


def test_outer_marker_preserves_schema_qualified_unaliased_bindings():
    raw = duckdb.connect()
    raw.execute("CREATE SCHEMA s; CREATE TABLE s.a(k INT); CREATE TABLE s.b(k INT)")
    raw.execute("INSERT INTO s.a VALUES(1),(2); INSERT INTO s.b VALUES(1),(3)")
    query = "SELECT s.a.k AS ak,s.b.k AS bk FROM s.a FULL JOIN s.b ON s.a.k=s.b.k"
    with dfc(raw) as conn:
        conn.register_policy(
            Policy(sources=["s.a", "s.b"], constraint="FALSE", on_fail=Resolution.REMOVE)
        )
        assert sorted(conn.execute(query).fetchall(), key=str) == sorted(
            [(2, None), (None, 3)], key=str
        )


@pytest.mark.parametrize("set_arm", [False, True])
def test_schema_qualified_outer_scan_binding_in_direct_and_union_arm(set_arm):
    raw = duckdb.connect()
    raw.execute("CREATE SCHEMA s")
    raw.execute("CREATE TABLE s.a(x INTEGER); CREATE TABLE s.b(x INTEGER)")
    raw.execute("INSERT INTO s.a VALUES (1),(2); INSERT INTO s.b VALUES (1)")
    query = "SELECT s.a.x AS x FROM s.a LEFT JOIN s.b ON s.a.x=s.b.x"
    if set_arm:
        query += " UNION ALL SELECT 3 AS x ORDER BY x"
    else:
        query += " ORDER BY s.a.x"
    expected_schema = raw.execute(query).description
    with dfc(raw) as conn:
        conn.register_policy(
            Policy(sources=["s.a", "s.b"], constraint="FALSE", on_fail=Resolution.REMOVE)
        )
        result = conn.execute(query)
        assert result.description == expected_schema
        assert result.fetchall() == ([(2,), (3,)] if set_arm else [(2,)])
