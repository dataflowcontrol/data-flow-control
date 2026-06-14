"""Regression test for meal_deduction_limit INSERT with nested aggregation."""

from __future__ import annotations

import duckdb
import sqlglot

from data_flow_control import Policy, Resolution, dfc
from data_flow_control.catalog import build_catalog_snapshot

MEAL_DEDUCTION_SINK_POLICY = Policy(
    sources=["Receipts"],
    sink="Expenses",
    constraint=(
        "COUNT(DISTINCT Receipts.cat) = 1 "
        "AND (ARBITRARY(Receipts.cat) != 'Meal' OR Expenses.biz_use <= 50.0)"
    ),
    on_fail=Resolution.KILL,
    description="meal_deduction_limit",
)

MEAL_DEDUCTION_OUTPUT_POLICY = Policy(
    sources=["Receipts"],
    constraint=(
        "COUNT(DISTINCT Receipts.cat) = 1 "
        "AND (ARBITRARY(Receipts.cat) != 'Meal' OR _OUTPUT_.biz_use <= 50.0)"
    ),
    on_fail=Resolution.KILL,
    description="meal_deduction_limit",
)

MEAL_DEDUCTION_QUERY = """
INSERT INTO Expenses (id, merchant, cost, biz_use, source_note)
WITH selected_receipts AS (
    SELECT * FROM (VALUES
        (1, 102),
        (2, 112),
        (3, 122),
        (4, 132),
        (5, 142),
        (6, 152),
        (7, 162),
        (8, 172),
        (9, 182),
        (10, 192)
    ) AS selected_receipts(selection_position, receipt_id)
),
candidate_receipts AS (
    SELECT
        Receipts.batch_id AS report_id,
        Receipts.id AS receipt_id,
        Receipts.cat,
        Receipts.merchant,
        Receipts.cost,
        selected_receipts.selection_position
    FROM Receipts
    JOIN selected_receipts
        ON selected_receipts.receipt_id = Receipts.id
),
rollups AS (
    SELECT
        report_id,
        merchant,
        MIN(selection_position) AS output_position,
        SUM(cost) AS cost
    FROM candidate_receipts
    GROUP BY report_id, merchant
)
SELECT
    report_id * 1000 + output_position AS id,
    merchant,
    cost,
    CASE WHEN merchant IN ('Cedar House 01-02') THEN 100.0 ELSE 50.0 END AS biz_use,
    '10_row_queries:q01' AS source_note
FROM rollups
ORDER BY output_position
"""

# Aggregate predicates (COUNT DISTINCT / ARBITRARY) are evaluated at the rollups
# GROUP BY scope. passant_kill() must wait for the outermost SELECT, which is the
# only scope where the projected biz_use output column is defined.
_MEAL_DEDUCTION_REWRITE_BODY = """
WITH selected_receipts AS (
    SELECT * FROM (VALUES
        (1, 102),
        (2, 112),
        (3, 122),
        (4, 132),
        (5, 142),
        (6, 152),
        (7, 162),
        (8, 172),
        (9, 182),
        (10, 192)
    ) AS selected_receipts(selection_position, receipt_id)
),
candidate_receipts AS (
    SELECT
        Receipts.batch_id AS report_id,
        Receipts.id AS receipt_id,
        Receipts.cat,
        Receipts.merchant,
        Receipts.cost,
        selected_receipts.selection_position
    FROM Receipts
    JOIN selected_receipts
        ON selected_receipts.receipt_id = Receipts.id
),
rollups AS (
    SELECT
        report_id,
        merchant,
        MIN(selection_position) AS output_position,
        SUM(cost) AS cost,
        COUNT(DISTINCT Receipts.cat) = 1 AS __passant_count_distinct_one,
        ARBITRARY(Receipts.cat) AS __passant_arbitrary_cat
    FROM candidate_receipts
    GROUP BY
        report_id,
        merchant
)
SELECT
    *
FROM (
    WITH t1 AS (
        SELECT
            report_id * 1000 + output_position AS id,
            merchant,
            cost,
            CASE
                WHEN merchant IN ('Cedar House 01-02') THEN 100.0
                ELSE 50.0
            END AS biz_use,
            '10_row_queries:q01' AS source_note,
            __passant_count_distinct_one
                AND (
                    __passant_arbitrary_cat <> 'Meal'
                    OR CASE
                        WHEN merchant IN ('Cedar House 01-02') THEN 100.0
                        ELSE 50.0
                    END <= 50.0
                ) AS __passant_policy_pass
        FROM rollups
    )
    SELECT
        t1.id,
        t1.merchant,
        t1.cost,
        t1.biz_use,
        t1.source_note
    FROM t1
    WHERE
        t1.__passant_policy_pass
        OR CASE
            WHEN NOT t1.__passant_policy_pass THEN passant_kill()
            ELSE TRUE
        END
) AS __passant_kill
ORDER BY
    id
"""

EXPECTED_MEAL_DEDUCTION_SINK_REWRITE = (
    "INSERT INTO Expenses (id, merchant, cost, biz_use, source_note)\n"
    + _MEAL_DEDUCTION_REWRITE_BODY
)

EXPECTED_MEAL_DEDUCTION_OUTPUT_REWRITE = _MEAL_DEDUCTION_REWRITE_BODY


def _pretty_sql(sql: str) -> str:
    return sqlglot.parse_one(sql, read="duckdb").sql(dialect="duckdb", pretty=True)


def _meal_deduction_rewriter(policy: Policy = MEAL_DEDUCTION_SINK_POLICY) -> dfc:
    conn = duckdb.connect()
    rewriter = dfc(conn, dialect="duckdb")
    rewriter.execute(
        "CREATE TABLE Receipts (batch_id INTEGER, id INTEGER, cat VARCHAR, merchant VARCHAR, cost DOUBLE)"
    )
    rewriter.execute(
        "CREATE TABLE Expenses (id INTEGER, merchant VARCHAR, cost DOUBLE, biz_use DOUBLE, source_note VARCHAR)"
    )
    rewriter.planner.sync_catalog(
        build_catalog_snapshot(
            dialect="duckdb",
            tables={
                "Receipts": {"columns": ["batch_id", "id", "cat", "merchant", "cost"]},
                "Expenses": {"columns": ["id", "merchant", "cost", "biz_use", "source_note"]},
            },
            aggregate_functions=[{"name": "arbitrary", "source": "introspected"}],
        )
    )
    rewriter.register_policy(policy)
    return rewriter


def test_meal_deduction_limit_insert_rewrite():
    rewriter = _meal_deduction_rewriter(MEAL_DEDUCTION_SINK_POLICY)
    transformed = rewriter.transform_query(MEAL_DEDUCTION_QUERY)
    assert _pretty_sql(transformed) == _pretty_sql(EXPECTED_MEAL_DEDUCTION_SINK_REWRITE)


def test_meal_deduction_limit_insert_rewrite_with_output_marker():
    rewriter = _meal_deduction_rewriter(MEAL_DEDUCTION_OUTPUT_POLICY)
    transformed = rewriter.transform_query(MEAL_DEDUCTION_QUERY)
    assert _pretty_sql(transformed) == _pretty_sql(EXPECTED_MEAL_DEDUCTION_OUTPUT_REWRITE)
