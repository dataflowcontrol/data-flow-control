"""Executable propagation contracts, independent of tool registration."""

import duckdb
import pytest

from data_flow_control.intermediates import Intermediate, IntermediateError, PolicyIntermediates


@pytest.fixture
def db():
    with duckdb.connect() as conn:
        conn.execute(
            "CREATE TABLE cases(id VARCHAR, category VARCHAR, priority VARCHAR, amount INT)"
        )
        conn.execute("""INSERT INTO cases VALUES
            ('A', 'Benefits', 'Low', 10), ('B', 'Discrimination', 'High', 20),
            ('C', 'Discrimination', 'Low', 30), ('D', NULL, NULL, NULL)""")
        yield conn


def capture(plan, where="TRUE", query=None):
    return plan.capture(
        "cases", query or f"SELECT * FROM cases WHERE {where}", identity_columns=["id"]
    )


def test_send_output_later_update_identity_and_success(db):
    plan = PolicyIntermediates(
        db,
        [
            Intermediate("case_id", "cases", "cases.id"),
            Intermediate("category", "cases", "cases.category"),
        ],
        required_sources=["cases"],
    )
    sent = capture(plan, "id = 'B'")
    # 1:many copies followed by a join/group diamond retain B once.
    update = plan.merge(sent, sent, plan.merge(sent))
    predicate = "case_id__cardinality = 1 AND case_id = target AND success"
    assert plan.evaluate(update, predicate, sink={"target": "B", "success": True})
    assert not plan.evaluate(update, predicate, sink={"target": "A", "success": True})
    assert not plan.evaluate(update, predicate, sink={"target": "B", "success": False})
    assert not plan.evaluate(plan.empty(), predicate, sink={"target": "B", "success": True})
    assert not plan.evaluate(
        plan.merge(sent, capture(plan, "id = 'A'")),
        predicate,
        sink={"target": "B", "success": True},
    )


def test_joint_expression_is_evaluated_before_aggregation(db):
    plan = PolicyIntermediates(
        db,
        [
            Intermediate(
                "routed", "cases", "category <> 'Discrimination' OR priority = 'High'", "bool_and"
            )
        ],
    )
    assert plan.values(capture(plan, "id IN ('A', 'B')")) == {"routed": True}
    assert plan.values(plan.merge(capture(plan, "id='A'"), capture(plan, "id='C'"))) == {
        "routed": False
    }


def test_unique_empty_null_single_conflict(db):
    plan = PolicyIntermediates(db, [Intermediate("cat", "cases", "category")])
    assert plan.values(plan.empty()) == {"cat": (0, None)}
    assert plan.values(capture(plan, "FALSE")) == {"cat": (0, None)}
    assert plan.values(capture(plan, "id='D'")) == {"cat": (0, None)}
    single = capture(plan, "id='B'")
    assert plan.values(plan.merge(single, capture(plan, "id='C'"))) == {
        "cat": (1, "Discrimination")
    }
    conflict = plan.merge(single, capture(plan, "id='A'"))
    assert plan.values(conflict) == {"cat": (2, None)}
    assert plan.values(plan.merge(conflict, single)) == {"cat": (2, None)}
    assert not plan.evaluate(single, "NULL")


@pytest.mark.parametrize(
    "aggregate, expected",
    [
        ("sum", 60),
        ("count", 3),
        ("avg", 20),
        ("min", 10),
        ("max", 30),
    ],
)
def test_merge_matches_one_shot_sql_with_nulls(db, aggregate, expected):
    plan = PolicyIntermediates(db, [Intermediate("value", "cases", "amount", aggregate)])
    states = [capture(plan, "id IN ('A', 'B')"), capture(plan, "id IN ('C', 'D')")]
    assert plan.values(plan.merge(*states)) == plan.values(capture(plan)) == {"value": expected}
    assert plan.values(plan.merge(states[0], states[0])) == plan.values(states[0])


def test_average_uses_weighted_partial_counts(db):
    plan = PolicyIntermediates(db, [Intermediate("value", "cases", "amount", "avg")])
    assert plan.values(plan.merge(capture(plan, "id='A'"), capture(plan, "id IN ('B','C')"))) == {
        "value": 20
    }


def test_join_multiplicity_deduplicates_identity_not_values(db):
    db.execute("UPDATE cases SET amount = 10 WHERE id = 'B'")
    plan = PolicyIntermediates(
        db,
        [Intermediate("total", "cases", "amount", "sum"), Intermediate("n", "cases", "*", "count")],
    )
    joined = capture(
        plan, query="SELECT cases.* FROM cases CROSS JOIN range(3) WHERE id IN ('A','B')"
    )
    assert plan.values(joined) == {"total": 20, "n": 2}


@pytest.mark.parametrize("aggregate", ["sum", "count", "avg"])
def test_partial_overlap_rejected_but_containment_is_exact(db, aggregate):
    plan = PolicyIntermediates(db, [Intermediate("value", "cases", "amount", aggregate)])
    ab, bc = capture(plan, "id IN ('A','B')"), capture(plan, "id IN ('B','C')")
    with pytest.raises(IntermediateError, match="overlapping"):
        plan.merge(ab, bc)
    all_rows = capture(plan)
    assert plan.values(plan.merge(ab, bc, all_rows)) == plan.values(all_rows)


def test_idempotent_overlap_is_safe(db):
    plan = PolicyIntermediates(
        db,
        [Intermediate("max", "cases", "amount", "max"), Intermediate("cat", "cases", "category")],
    )
    result = plan.merge(capture(plan, "id IN ('A','B')"), capture(plan, "id IN ('B','C')"))
    assert plan.values(result) == {"max": 30, "cat": (2, None)}


def test_multiple_sources_and_required_applicability(db):
    db.execute("CREATE TABLE observations(id INT, success BOOLEAN)")
    db.execute("INSERT INTO observations VALUES (1, TRUE)")
    specs = [
        Intermediate("case_id", "cases", "id"),
        Intermediate("sent", "observations", "success", "bool_and"),
    ]
    plan = PolicyIntermediates(db, specs)
    case = capture(plan, "id='B'")
    assert plan.evaluate(case, "TRUE") is None
    observation = plan.capture(
        "observations", "SELECT * FROM observations", identity_columns=["id"]
    )
    joined = plan.merge(case, observation)
    assert plan.evaluate(joined, "case_id__cardinality = 1 AND case_id = 'B' AND sent")
    required = PolicyIntermediates(db, specs, required_sources=["observations"])
    assert required.evaluate(capture(required, "id='B'"), "TRUE") is False


def test_source_presence_without_expression_and_empty_group(db):
    plan = PolicyIntermediates(db, [], sources=["cases"], required_sources=["cases"])
    assert plan.evaluate(capture(plan, "id='B'"), "TRUE")
    assert not plan.evaluate(capture(plan, "FALSE"), "TRUE")
    assert plan.values(plan.merge()) == {}


def test_reject_inconsistent_and_null_identity(db):
    plan = PolicyIntermediates(db, [Intermediate("value", "cases", "amount", "sum")])
    with pytest.raises(IntermediateError, match="inconsistent"):
        capture(plan, query="SELECT 'A' id, 10 amount UNION ALL SELECT 'A', 20")
    with pytest.raises(IntermediateError, match="NULL"):
        capture(plan, query="SELECT NULL id, 10 amount")


def test_state_plan_isolation(db):
    a = PolicyIntermediates(db, [Intermediate("id", "cases", "id")])
    b = PolicyIntermediates(db, [Intermediate("id", "cases", "id")])
    with pytest.raises(IntermediateError, match="another policy"):
        b.merge(capture(a))


def test_expression_and_query_rejections(db):
    with pytest.raises(IntermediateError, match="Unsupported"):
        Intermediate("value", "cases", "amount", "median")
    with pytest.raises(IntermediateError, match="contributor-local"):
        Intermediate("value", "cases", "SUM(amount)")
    plan = PolicyIntermediates(db, [Intermediate("id", "cases", "id")])
    with pytest.raises(IntermediateError, match="SELECT"):
        capture(plan, query="DELETE FROM cases")
    with pytest.raises(IntermediateError, match="shadows"):
        plan.evaluate(capture(plan), "TRUE", sink={"id": "fake"})
    with pytest.raises(IntermediateError, match="scalar"):
        plan.evaluate(capture(plan), "EXISTS(SELECT 1)")
