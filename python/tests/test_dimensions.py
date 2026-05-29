"""DIMENSION policy relations: parsing, catalog validation, rewrite, and execution."""

from passant import Policy, Resolution, dfc


def test_policy_accepts_dimension_tables_and_aliases():
    policy = Policy(
        sources=["foo"],
        dimensions=["catalog_users", "catalog_roles"],
        dimension_aliases={"u": "catalog_users", "r": "catalog_roles"},
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    assert policy.dimensions == ["catalog_users", "catalog_roles"]
    assert policy.dimension_aliases == {"u": "catalog_users", "r": "catalog_roles"}


def test_rewrite_injects_dimension_join_on_fk_equality():
    conn = duckdb_connect()
    conn.execute("CREATE TABLE foo (id INTEGER, region_id INTEGER)")
    conn.execute("CREATE TABLE regions (id INTEGER, code VARCHAR)")
    conn.execute("INSERT INTO foo VALUES (1, 10), (2, 20)")
    conn.execute("INSERT INTO regions VALUES (10, 'US'), (20, 'EU')")
    rewriter = dfc(conn)
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            dimensions=["regions"],
            constraint=(
                "max(foo.id) > 0 AND regions.code = 'US' AND max(foo.region_id) = regions.id"
            ),
            on_fail=Resolution.REMOVE,
        )
    )
    sql = rewriter.transform_query("SELECT foo.id FROM foo")
    assert "JOIN regions" in sql
    join_clause = sql.split("JOIN regions", 1)[1].split("WHERE", 1)[0]
    assert "max(foo.region_id)" not in join_clause
    rows = rewriter.fetchall("SELECT foo.id FROM foo ORDER BY 1")
    assert rows == [(1,)]
    summary = rewriter.last_statement_rewrite_summary()
    assert any("multiplicity" in warning for warning in summary.warnings)


def test_dimension_alias_in_constraint():
    conn = duckdb_connect()
    conn.execute("CREATE TABLE foo (id INTEGER)")
    conn.execute("CREATE TABLE session_user (user_id INTEGER)")
    conn.execute("INSERT INTO foo VALUES (1), (2)")
    conn.execute("INSERT INTO session_user VALUES (1)")
    rewriter = dfc(conn)
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            dimensions=["session_user"],
            dimension_aliases={"u": "session_user"},
            constraint="max(foo.id) = 1 AND u.user_id = 1",
            on_fail=Resolution.REMOVE,
        )
    )
    rows = rewriter.fetchall("SELECT foo.id FROM foo ORDER BY 1")
    assert rows == [(1,)]


def test_dimension_skipped_without_join_key_fails_closed():
    conn = duckdb_connect()
    conn.execute("CREATE TABLE foo (id INTEGER)")
    conn.execute("CREATE TABLE regions (id INTEGER, code VARCHAR)")
    conn.execute("INSERT INTO foo VALUES (1), (2)")
    conn.execute("INSERT INTO regions VALUES (10, 'US'), (20, 'EU')")
    rewriter = dfc(conn)
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            dimensions=["regions"],
            constraint="max(foo.id) > 0 AND regions.code = 'US'",
            on_fail=Resolution.REMOVE,
        )
    )
    rows = rewriter.fetchall("SELECT foo.id FROM foo ORDER BY 1")
    assert rows == []
    summary = rewriter.last_statement_rewrite_summary()
    assert any("was not joined" in warning for warning in summary.warnings)


def test_dimension_skipped_without_join_key_warns():
    conn = duckdb_connect()
    conn.execute("CREATE TABLE foo (id INTEGER)")
    conn.execute("CREATE TABLE regions (id INTEGER, code VARCHAR)")
    conn.execute("INSERT INTO regions VALUES (10, 'US'), (20, 'EU')")
    rewriter = dfc(conn)
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            dimensions=["regions"],
            constraint="max(foo.id) > 0 AND regions.code = 'US'",
            on_fail=Resolution.REMOVE,
        )
    )
    rewriter.transform_query("SELECT foo.id FROM foo")
    summary = rewriter.last_statement_rewrite_summary()
    assert any("was not joined" in warning for warning in summary.warnings)


def duckdb_connect():
    import duckdb

    return duckdb.connect()
