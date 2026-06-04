"""TPC-H rewrite regression tests for Passant.

Each test asserts the complete rewritten SQL output for one query.
Excluded queries (non-monotonic): Q02, Q11, Q13, Q15, Q16, Q17, Q20, Q21, Q22.
"""

from __future__ import annotations

import pathlib

import duckdb
import pytest
import sqlglot

from data_flow_control import Policy, Resolution, dfc

lineitem_policy = Policy(
    sources=["lineitem"],
    constraint="max(lineitem.l_quantity) >= 1",
    on_fail=Resolution.REMOVE,
)

TPCH_QUERY_NUMS = [1, 3, 4, 5, 6, 7, 8, 9, 10, 12, 14, 18, 19]


@pytest.fixture
def tpch_rewriter():
    rewriter = dfc(duckdb.connect())
    # Only a minimal source table is needed for source registration validation.
    rewriter.execute("CREATE TABLE lineitem(l_quantity INTEGER)")
    rewriter.register_policy(lineitem_policy)
    yield rewriter
    rewriter.close()


@pytest.fixture(scope="module")
def tpch_sf001_rewriter():
    conn = duckdb.connect()
    conn.execute("INSTALL tpch")
    conn.execute("LOAD tpch")
    conn.execute("CALL dbgen(sf=0.01)")
    rewriter = dfc(conn)
    rewriter.register_policy(lineitem_policy)
    yield rewriter
    rewriter.close()


@pytest.mark.parametrize("query_num", TPCH_QUERY_NUMS)
def test_tpch_rewritten_query_executes_on_sf001(tpch_sf001_rewriter, query_num: int):
    query = load_tpch_query(query_num)
    transformed = tpch_sf001_rewriter.transform_query(query)
    tpch_sf001_rewriter.adapter.connection.execute(transformed).fetchall()


def load_tpch_query(query_num: int) -> str:
    benchmarks_dir = pathlib.Path(__file__).resolve().parents[3] / "benchmarks" / "tpch" / "queries"
    query_file = benchmarks_dir / f"q{query_num:02d}.sql"
    if not query_file.exists():
        raise FileNotFoundError(f"TPC-H query {query_num} not found at {query_file}")
    return query_file.read_text()


def assert_tpch_rewrite(tpch_rewriter, query_num: int, expected_sql: str):
    query = load_tpch_query(query_num)
    transformed = tpch_rewriter.transform_query(query)
    assert _pretty_sql(transformed) == _pretty_sql(expected_sql)


def _pretty_sql(sql: str) -> str:
    return sqlglot.parse_one(sql, read="duckdb").sql(dialect="duckdb", pretty=True)


def test_tpch_q01_rewrite_sql(tpch_rewriter):
    assert_tpch_rewrite(
        tpch_rewriter,
        1,
        """
SELECT
  l_returnflag,
  l_linestatus,
  SUM(l_quantity) AS sum_qty,
  SUM(l_extendedprice) AS sum_base_price,
  SUM(l_extendedprice * (
    1 - l_discount
  )) AS sum_disc_price,
  SUM(l_extendedprice * (
    1 - l_discount
  ) * (
    1 + l_tax
  )) AS sum_charge,
  AVG(l_quantity) AS avg_qty,
  AVG(l_extendedprice) AS avg_price,
  AVG(l_discount) AS avg_disc,
  COUNT(*) AS count_order
FROM lineitem
WHERE
  l_shipdate <= CAST('1998-09-02' AS DATE)
GROUP BY
  l_returnflag,
  l_linestatus
HAVING
  MAX(lineitem.l_quantity) >= 1
ORDER BY
  l_returnflag,
  l_linestatus
""",
    )


def test_tpch_q03_rewrite_sql(tpch_rewriter):
    assert_tpch_rewrite(
        tpch_rewriter,
        3,
        """
WITH __passant_limited AS (
  SELECT
    l_orderkey,
    SUM(l_extendedprice * (
      1 - l_discount
    )) AS revenue,
    o_orderdate,
    o_shippriority,
    MAX(lineitem.l_quantity) AS __passant_filter_agg_0
  FROM customer, orders, lineitem
  WHERE
    c_mktsegment = 'BUILDING'
    AND c_custkey = o_custkey
    AND l_orderkey = o_orderkey
    AND o_orderdate < CAST('1995-03-15' AS DATE)
    AND l_shipdate > CAST('1995-03-15' AS DATE)
  GROUP BY
    l_orderkey,
    o_orderdate,
    o_shippriority
  ORDER BY
    revenue DESC,
    o_orderdate
  LIMIT 10
)
SELECT
  l_orderkey,
  revenue,
  o_orderdate,
  o_shippriority
FROM __passant_limited
WHERE
  __passant_filter_agg_0 >= 1
""",
    )


def test_tpch_q04_rewrite_sql(tpch_rewriter):
    assert_tpch_rewrite(
        tpch_rewriter,
        4,
        """
SELECT
  o_orderpriority,
  COUNT(*) AS order_count
FROM orders
JOIN (
  SELECT
    l_orderkey AS l_orderkey,
    MAX(l_quantity) AS agg_0
  FROM lineitem
  WHERE
    l_commitdate < l_receiptdate
  GROUP BY
    l_orderkey
) AS exists_subquery
  ON o_orderkey = exists_subquery.l_orderkey
WHERE
  o_orderdate >= CAST('1993-07-01' AS DATE)
  AND o_orderdate < CAST('1993-10-01' AS DATE)
GROUP BY
  o_orderpriority
HAVING
  MAX(exists_subquery.agg_0) >= 1
ORDER BY
  o_orderpriority
""",
    )


def test_tpch_q05_rewrite_sql(tpch_rewriter):
    assert_tpch_rewrite(
        tpch_rewriter,
        5,
        """
SELECT
  n_name,
  SUM(l_extendedprice * (
    1 - l_discount
  )) AS revenue
FROM customer, orders, lineitem, supplier, nation, region
WHERE
  c_custkey = o_custkey
  AND l_orderkey = o_orderkey
  AND l_suppkey = s_suppkey
  AND c_nationkey = s_nationkey
  AND s_nationkey = n_nationkey
  AND n_regionkey = r_regionkey
  AND r_name = 'ASIA'
  AND o_orderdate >= CAST('1994-01-01' AS DATE)
  AND o_orderdate < CAST('1995-01-01' AS DATE)
GROUP BY
  n_name
HAVING
  MAX(lineitem.l_quantity) >= 1
ORDER BY
  revenue DESC
""",
    )


def test_tpch_q06_rewrite_sql(tpch_rewriter):
    assert_tpch_rewrite(
        tpch_rewriter,
        6,
        """
SELECT
  SUM(l_extendedprice * l_discount) AS revenue
FROM lineitem
WHERE
  l_shipdate >= CAST('1994-01-01' AS DATE)
  AND l_shipdate < CAST('1995-01-01' AS DATE)
  AND l_discount BETWEEN 0.05 AND 0.07
  AND l_quantity < 24
HAVING
  MAX(lineitem.l_quantity) >= 1
""",
    )


def test_tpch_q07_rewrite_sql(tpch_rewriter):
    assert_tpch_rewrite(
        tpch_rewriter,
        7,
        """
SELECT
  supp_nation,
  cust_nation,
  l_year,
  SUM(volume) AS revenue
FROM (
  SELECT
    n1.n_name AS supp_nation,
    n2.n_name AS cust_nation,
    EXTRACT(YEAR FROM l_shipdate) AS l_year,
    l_extendedprice * (
      1 - l_discount
    ) AS volume,
    lineitem.l_quantity AS __passant_filter_policy_0_lineitem_l_quantity
  FROM supplier, lineitem, orders, customer, nation AS n1, nation AS n2
  WHERE
    s_suppkey = l_suppkey
    AND o_orderkey = l_orderkey
    AND c_custkey = o_custkey
    AND s_nationkey = n1.n_nationkey
    AND c_nationkey = n2.n_nationkey
    AND (
      (
        n1.n_name = 'FRANCE' AND n2.n_name = 'GERMANY'
      )
      OR (
        n1.n_name = 'GERMANY' AND n2.n_name = 'FRANCE'
      )
    )
    AND l_shipdate BETWEEN CAST('1995-01-01' AS DATE) AND CAST('1996-12-31' AS DATE)
) AS shipping
GROUP BY
  supp_nation,
  cust_nation,
  l_year
HAVING
  MAX(shipping.__passant_filter_policy_0_lineitem_l_quantity) >= 1
ORDER BY
  supp_nation,
  cust_nation,
  l_year
""",
    )


def test_tpch_q08_rewrite_sql(tpch_rewriter):
    assert_tpch_rewrite(
        tpch_rewriter,
        8,
        """
SELECT
  o_year,
  SUM(CASE WHEN nation = 'BRAZIL' THEN volume ELSE 0 END) / SUM(volume) AS mkt_share
FROM (
  SELECT
    EXTRACT(YEAR FROM o_orderdate) AS o_year,
    l_extendedprice * (
      1 - l_discount
    ) AS volume,
    n2.n_name AS nation,
    lineitem.l_quantity AS __passant_filter_policy_0_lineitem_l_quantity
  FROM part, supplier, lineitem, orders, customer, nation AS n1, nation AS n2, region
  WHERE
    p_partkey = l_partkey
    AND s_suppkey = l_suppkey
    AND l_orderkey = o_orderkey
    AND o_custkey = c_custkey
    AND c_nationkey = n1.n_nationkey
    AND n1.n_regionkey = r_regionkey
    AND r_name = 'AMERICA'
    AND s_nationkey = n2.n_nationkey
    AND o_orderdate BETWEEN CAST('1995-01-01' AS DATE) AND CAST('1996-12-31' AS DATE)
    AND p_type = 'ECONOMY ANODIZED STEEL'
) AS all_nations
GROUP BY
  o_year
HAVING
  MAX(all_nations.__passant_filter_policy_0_lineitem_l_quantity) >= 1
ORDER BY
  o_year
""",
    )


def test_tpch_q09_rewrite_sql(tpch_rewriter):
    assert_tpch_rewrite(
        tpch_rewriter,
        9,
        """
SELECT
  nation,
  o_year,
  SUM(amount) AS sum_profit
FROM (
  SELECT
    n_name AS nation,
    EXTRACT(YEAR FROM o_orderdate) AS o_year,
    l_extendedprice * (
      1 - l_discount
    ) - ps_supplycost * l_quantity AS amount,
    lineitem.l_quantity AS __passant_filter_policy_0_lineitem_l_quantity
  FROM part, supplier, lineitem, partsupp, orders, nation
  WHERE
    s_suppkey = l_suppkey
    AND ps_suppkey = l_suppkey
    AND ps_partkey = l_partkey
    AND p_partkey = l_partkey
    AND o_orderkey = l_orderkey
    AND s_nationkey = n_nationkey
    AND p_name LIKE '%green%'
) AS profit
GROUP BY
  nation,
  o_year
HAVING
  MAX(profit.__passant_filter_policy_0_lineitem_l_quantity) >= 1
ORDER BY
  nation,
  o_year DESC
""",
    )


def test_tpch_q10_rewrite_sql(tpch_rewriter):
    assert_tpch_rewrite(
        tpch_rewriter,
        10,
        """
WITH __passant_limited AS (
  SELECT
    c_custkey,
    c_name,
    SUM(l_extendedprice * (
      1 - l_discount
    )) AS revenue,
    c_acctbal,
    n_name,
    c_address,
    c_phone,
    c_comment,
    MAX(lineitem.l_quantity) AS __passant_filter_agg_0
  FROM customer, orders, lineitem, nation
  WHERE
    c_custkey = o_custkey
    AND l_orderkey = o_orderkey
    AND o_orderdate >= CAST('1993-10-01' AS DATE)
    AND o_orderdate < CAST('1994-01-01' AS DATE)
    AND l_returnflag = 'R'
    AND c_nationkey = n_nationkey
  GROUP BY
    c_custkey,
    c_name,
    c_acctbal,
    c_phone,
    n_name,
    c_address,
    c_comment
  ORDER BY
    revenue DESC
  LIMIT 20
)
SELECT
  c_custkey,
  c_name,
  revenue,
  c_acctbal,
  n_name,
  c_address,
  c_phone,
  c_comment
FROM __passant_limited
WHERE
  __passant_filter_agg_0 >= 1
""",
    )


def test_tpch_q12_rewrite_sql(tpch_rewriter):
    assert_tpch_rewrite(
        tpch_rewriter,
        12,
        """
SELECT
  l_shipmode,
  SUM(
    CASE
      WHEN o_orderpriority = '1-URGENT' OR o_orderpriority = '2-HIGH'
      THEN 1
      ELSE 0
    END
  ) AS high_line_count,
  SUM(
    CASE
      WHEN o_orderpriority <> '1-URGENT' AND o_orderpriority <> '2-HIGH'
      THEN 1
      ELSE 0
    END
  ) AS low_line_count
FROM orders, lineitem
WHERE
  o_orderkey = l_orderkey
  AND l_shipmode IN ('MAIL', 'SHIP')
  AND l_commitdate < l_receiptdate
  AND l_shipdate < l_commitdate
  AND l_receiptdate >= CAST('1994-01-01' AS DATE)
  AND l_receiptdate < CAST('1995-01-01' AS DATE)
GROUP BY
  l_shipmode
HAVING
  MAX(lineitem.l_quantity) >= 1
ORDER BY
  l_shipmode
""",
    )


def test_tpch_q14_rewrite_sql(tpch_rewriter):
    assert_tpch_rewrite(
        tpch_rewriter,
        14,
        """
SELECT
  100.00 * SUM(
    CASE
      WHEN p_type LIKE 'PROMO%'
      THEN l_extendedprice * (
        1 - l_discount
      )
      ELSE 0
    END
  ) / SUM(l_extendedprice * (
    1 - l_discount
  )) AS promo_revenue
FROM lineitem, part
WHERE
  l_partkey = p_partkey
  AND l_shipdate >= CAST('1995-09-01' AS DATE)
  AND l_shipdate < CAST('1995-10-01' AS DATE)
HAVING
  MAX(lineitem.l_quantity) >= 1
""",
    )


def test_tpch_q18_rewrite_sql(tpch_rewriter):
    assert_tpch_rewrite(
        tpch_rewriter,
        18,
        """
WITH __passant_limited AS (
  SELECT
    c_name,
    c_custkey,
    o_orderkey,
    o_orderdate,
    o_totalprice,
    SUM(l_quantity) AS sum_l_quantity,
    MAX(in_subquery.__passant_filter_in_metric_0) AS __passant_filter_filter_agg_0,
    MAX(lineitem.l_quantity) AS __passant_filter_agg_0
  FROM customer, orders, lineitem
  JOIN (
    SELECT
      l_orderkey,
      MAX(lineitem.l_quantity) AS __passant_filter_in_metric_0
    FROM lineitem
    GROUP BY
      l_orderkey
    HAVING
      SUM(l_quantity) > 300
  ) AS in_subquery
    ON o_orderkey = in_subquery.l_orderkey
  WHERE
    c_custkey = o_custkey AND o_orderkey = lineitem.l_orderkey
  GROUP BY
    c_name,
    c_custkey,
    o_orderkey,
    o_orderdate,
    o_totalprice
  ORDER BY
    o_totalprice DESC,
    o_orderdate
  LIMIT 100
)
SELECT
  c_name,
  c_custkey,
  o_orderkey,
  o_orderdate,
  o_totalprice,
  sum_l_quantity
FROM __passant_limited
WHERE
  __passant_filter_agg_0 >= 1 AND __passant_filter_filter_agg_0 >= 1
""",
    )


def test_tpch_q19_rewrite_sql(tpch_rewriter):
    assert_tpch_rewrite(
        tpch_rewriter,
        19,
        """
SELECT
  SUM(l_extendedprice * (
    1 - l_discount
  )) AS revenue
FROM lineitem, part
WHERE
  (
    p_partkey = l_partkey
    AND p_brand = 'Brand#12'
    AND p_container IN ('SM CASE', 'SM BOX', 'SM PACK', 'SM PKG')
    AND l_quantity >= 1
    AND l_quantity <= 1 + 10
    AND p_size BETWEEN 1 AND 5
    AND l_shipmode IN ('AIR', 'AIR REG')
    AND l_shipinstruct = 'DELIVER IN PERSON'
  )
  OR (
    p_partkey = l_partkey
    AND p_brand = 'Brand#23'
    AND p_container IN ('MED BAG', 'MED BOX', 'MED PKG', 'MED PACK')
    AND l_quantity >= 10
    AND l_quantity <= 10 + 10
    AND p_size BETWEEN 1 AND 10
    AND l_shipmode IN ('AIR', 'AIR REG')
    AND l_shipinstruct = 'DELIVER IN PERSON'
  )
  OR (
    p_partkey = l_partkey
    AND p_brand = 'Brand#34'
    AND p_container IN ('LG CASE', 'LG BOX', 'LG PACK', 'LG PKG')
    AND l_quantity >= 20
    AND l_quantity <= 20 + 10
    AND p_size BETWEEN 1 AND 15
    AND l_shipmode IN ('AIR', 'AIR REG')
    AND l_shipinstruct = 'DELIVER IN PERSON'
  )
HAVING
  MAX(lineitem.l_quantity) >= 1
""",
    )
