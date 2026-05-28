"""Additional query-shape coverage beyond `test_rewrite.py`."""

from __future__ import annotations

from passant import Policy, Resolution


class TestJoinTypes:
    def test_cross_join(self, rewriter):
        policy = Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
        rewriter.register_policy(policy)
        transformed = rewriter.transform_query("SELECT foo.id FROM foo CROSS JOIN baz")
        assert "CROSS JOIN" in transformed.upper()
        assert "foo.id > 1" in transformed
        assert rewriter.raw_connection.execute(transformed).fetchall() is not None

    def test_select_distinct_with_policy(self, rewriter):
        policy = Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
        rewriter.register_policy(policy)
        transformed = rewriter.transform_query("SELECT DISTINCT id FROM foo")
        assert "DISTINCT" in transformed.upper()
        assert "foo.id > 1" in transformed
        assert len(rewriter.raw_connection.execute(transformed).fetchall()) == 2


class TestWindowFunctions:
    def test_window_function_scan_with_policy(self, rewriter):
        policy = Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
        rewriter.register_policy(policy)
        query = "SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn FROM foo"
        transformed = rewriter.transform_query(query)
        assert "ROW_NUMBER() OVER" in transformed
        assert "foo.id > 1" in transformed
        assert "HAVING" not in transformed.upper()
        assert len(rewriter.raw_connection.execute(transformed).fetchall()) == 2


class TestInSubqueries:
    def test_in_with_list(self, rewriter):
        policy = Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
        rewriter.register_policy(policy)
        transformed = rewriter.transform_query("SELECT id FROM foo WHERE id IN (1, 2, 3)")
        assert "IN (1, 2, 3)" in transformed
        assert "foo.id > 1" in transformed
        assert len(rewriter.raw_connection.execute(transformed).fetchall()) == 2


class TestCorrelatedSubqueries:
    def test_correlated_subquery_in_select(self, rewriter):
        policy = Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
        rewriter.register_policy(policy)
        query = "SELECT id, (SELECT COUNT(*) FROM baz WHERE baz.x = foo.id) AS count FROM foo"
        transformed = rewriter.transform_query(query)
        assert "foo.id > 1" in transformed
        assert "HAVING" not in transformed.upper()
        assert len(rewriter.raw_connection.execute(transformed).fetchall()) == 2

    def test_correlated_subquery_in_where(self, rewriter):
        policy = Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
        rewriter.register_policy(policy)
        query = "SELECT id FROM foo WHERE id = (SELECT x FROM baz WHERE baz.x = foo.id)"
        transformed = rewriter.transform_query(query)
        assert "foo.id > 1" in transformed
        assert rewriter.raw_connection.execute(transformed).fetchall() is not None


class TestExistsSubqueries:
    def test_exists_subquery_with_policy_on_outer_table(self, rewriter):
        rewriter.execute("CREATE TABLE orders (o_orderkey INTEGER)")
        rewriter.execute("INSERT INTO orders VALUES (1), (2)")
        rewriter.execute("CREATE TABLE lineitem (l_orderkey INTEGER, l_quantity INTEGER)")
        rewriter.execute("INSERT INTO lineitem VALUES (1, 10), (2, 5)")

        policy = Policy(
            sources=["orders"],
            constraint="max(orders.o_orderkey) >= 1",
            on_fail=Resolution.REMOVE,
        )
        rewriter.register_policy(policy)
        query = (
            "SELECT o_orderkey FROM orders "
            "WHERE EXISTS (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey)"
        )
        transformed = rewriter.transform_query(query)
        assert "EXISTS" in transformed.upper()
        assert "o_orderkey" in transformed
        assert rewriter.raw_connection.execute(transformed).fetchall() is not None

    def test_exists_subquery_with_policy_on_inner_table_rewrites_to_join(self, rewriter):
        rewriter.execute(
            "CREATE TABLE orders (o_orderkey INTEGER, o_orderdate DATE, o_orderpriority VARCHAR)"
        )
        rewriter.execute(
            "INSERT INTO orders VALUES (1, '1993-07-15', '1-URGENT'), (2, '1993-08-15', '2-HIGH')"
        )
        rewriter.execute(
            "CREATE TABLE lineitem (l_orderkey INTEGER, l_commitdate DATE, l_receiptdate DATE, l_quantity INTEGER)"
        )
        rewriter.execute(
            "INSERT INTO lineitem VALUES (1, '1993-07-10', '1993-07-20', 10), (2, '1993-08-10', '1993-08-05', 5)"
        )

        policy = Policy(
            sources=["lineitem"],
            constraint="max(lineitem.l_quantity) >= 1",
            on_fail=Resolution.REMOVE,
        )
        rewriter.register_policy(policy)
        query = """SELECT o_orderpriority, COUNT(*) AS order_count
FROM orders
WHERE o_orderdate >= CAST('1993-07-01' AS DATE)
  AND o_orderdate < CAST('1993-10-01' AS DATE)
  AND EXISTS (
    SELECT * FROM lineitem
    WHERE l_orderkey = o_orderkey AND l_commitdate < l_receiptdate
  )
GROUP BY o_orderpriority
ORDER BY o_orderpriority"""
        transformed = rewriter.transform_query(query)
        assert "base_query" not in transformed.lower()
        assert "exists_subquery" in transformed.lower()
        assert "having" in transformed.lower()
        assert rewriter.raw_connection.execute(transformed).fetchall() is not None


class TestSubqueryWithMissingColumns:
    def test_subquery_missing_policy_column(self, rewriter):
        policy = Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
        rewriter.register_policy(policy)
        query = "SELECT sub.name FROM (SELECT foo.name FROM foo) AS sub"
        transformed = rewriter.transform_query(query)
        assert "foo.id" in transformed or "sub.id" in transformed
        assert len(rewriter.raw_connection.execute(transformed).fetchall()) == 2


class TestMultipleCTEs:
    def test_nested_ctes_recurses_into_source(self, rewriter):
        policy = Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
        rewriter.register_policy(policy)
        query = """
        WITH cte1 AS (SELECT id FROM foo),
             cte2 AS (SELECT id FROM cte1 WHERE id > 1)
        SELECT * FROM cte2
        """
        transformed = rewriter.transform_query(query)
        assert "foo.id > 1" in transformed
        assert rewriter.raw_connection.execute(transformed).fetchall() is not None

    def test_multiple_ctes_with_joins(self, rewriter):
        policy = Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
        rewriter.register_policy(policy)
        query = """
        WITH cte1 AS (SELECT id FROM foo),
             cte2 AS (SELECT x FROM baz)
        SELECT cte1.id, cte2.x FROM cte1 JOIN cte2 ON cte1.id = cte2.x
        """
        transformed = rewriter.transform_query(query)
        assert "foo.id > 1" in transformed
        assert rewriter.raw_connection.execute(transformed).fetchall() is not None


class TestInsertStatements:
    def test_insert_values_passes_through(self, rewriter):
        rewriter.execute("CREATE TABLE dest (id INTEGER)")
        policy = Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
        rewriter.register_policy(policy)
        transformed = rewriter.transform_query("INSERT INTO dest VALUES (1), (2)")
        assert transformed == "INSERT INTO dest VALUES (1), (2)"

    def test_insert_with_sink_only_policy_remove(self, rewriter):
        rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
        policy = Policy(
            sources=[],
            sink="reports",
            constraint="reports.status = 'approved'",
            on_fail=Resolution.REMOVE,
        )
        rewriter.register_policy(policy)
        query = "INSERT INTO reports SELECT id, 'pending' FROM foo"
        transformed = rewriter.transform_query(query)
        assert "'pending' = 'approved'" in transformed

    def test_insert_with_sink_only_policy_kill(self, rewriter):
        rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
        policy = Policy(
            sources=[],
            sink="reports",
            constraint="reports.status = 'approved'",
            on_fail=Resolution.KILL,
        )
        rewriter.register_policy(policy)
        query = "INSERT INTO reports SELECT 1, 'pending' FROM foo WHERE id = 1"
        transformed = rewriter.transform_query(query)
        assert "KILL()" in transformed.upper() or "kill()" in transformed


class TestPolicyRowDropping:
    def test_policy_drops_rows_with_ne_constraint(self, rewriter):
        policy = Policy(
            sources=["foo"],
            constraint="max(foo.id) != 2",
            on_fail=Resolution.REMOVE,
        )
        rewriter.register_policy(policy)
        transformed = rewriter.transform_query("SELECT id, name FROM foo ORDER BY id")
        result = rewriter.raw_connection.execute(transformed).fetchall()
        assert result == [(1, "Alice"), (3, "Charlie")]

    def test_policy_drops_rows_with_or_constraint(self, rewriter):
        policy = Policy(
            sources=["foo"],
            constraint="max(foo.id) = 1 OR max(foo.id) = 3",
            on_fail=Resolution.REMOVE,
        )
        rewriter.register_policy(policy)
        transformed = rewriter.transform_query("SELECT id, name FROM foo ORDER BY id")
        result = rewriter.raw_connection.execute(transformed).fetchall()
        assert result == [(1, "Alice"), (3, "Charlie")]

    def test_policy_scan_with_approx_count_distinct(self, rewriter):
        policy = Policy(
            sources=["foo"],
            constraint="approx_count_distinct(foo.id) = 1",
            on_fail=Resolution.REMOVE,
        )
        rewriter.register_policy(policy)
        transformed = rewriter.transform_query("SELECT id FROM foo")
        assert "1 = 1" in transformed
        assert len(rewriter.raw_connection.execute(transformed).fetchall()) == 3


class TestMultiSourceRewrites:
    def test_multi_source_scan_with_left_join(self, rewriter):
        policy = Policy(
            sources=["foo", "baz"],
            constraint="max(foo.id) >= 2 AND max(baz.x) <= 20",
            on_fail=Resolution.REMOVE,
        )
        rewriter.register_policy(policy)
        query = "SELECT foo.id, baz.x FROM foo LEFT JOIN baz ON foo.id = baz.x"
        transformed = rewriter.transform_query(query)
        assert "foo.id >= 2" in transformed
        assert "baz.x <= 20" in transformed
        assert rewriter.raw_connection.execute(transformed).fetchall() is not None
