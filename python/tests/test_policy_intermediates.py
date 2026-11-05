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


def test_compile_policy_carries_multiple_expressions_to_later_sink(db):
    from data_flow_control import Policy
    from data_flow_control.intermediate_policy import compile_policy_intermediates

    policy = Policy.from_pgn("""SOURCE REQUIRED cases AS c SINK updates AS u
        CONSTRAINT c.id = u.case_id AND
        BOOL_AND(c.category <> 'Discrimination' OR c.priority = 'High') AND u.success
        ON FAIL REMOVE""")
    compiled = compile_policy_intermediates(db, policy)
    state = capture(compiled.plan, "id='B'")
    assert compiled.evaluate(
        compiled.plan.merge(state, state), sink={"case_id": "B", "success": True}
    )
    assert not compiled.evaluate(state, sink={"case_id": "A", "success": True})
    assert not compiled.evaluate(compiled.plan.empty(), sink={"case_id": "B", "success": True})


def test_compile_joint_unique_expression_and_aggregate_arithmetic(db):
    from data_flow_control import Policy
    from data_flow_control.intermediate_policy import compile_policy_intermediates

    policy = Policy.from_pgn("""SOURCE cases SINK target CONSTRAINT
        cases.amount - cases.amount = 0 AND AVG(cases.amount) = target.expected
        AND SUM(cases.amount) = 60 ON FAIL REMOVE""")
    compiled = compile_policy_intermediates(db, policy)
    state = compiled.plan.merge(
        capture(compiled.plan, "id='A'"), capture(compiled.plan, "id IN ('B','C','D')")
    )
    assert compiled.evaluate(state, sink={"expected": 20})


@pytest.mark.parametrize(
    "text, expected", [("UNIQUE cases.category", False), ("NOT UNIQUE cases.category", True)]
)
def test_compile_unique_cardinality(db, text, expected):
    from data_flow_control import Policy
    from data_flow_control.intermediate_policy import compile_policy_intermediates

    compiled = compile_policy_intermediates(
        db, Policy.from_pgn(f"SOURCE cases CONSTRAINT {text} ON FAIL REMOVE")
    )
    assert compiled.evaluate(capture(compiled.plan)) is expected


def test_compile_rejects_noncompact_or_cross_source_aggregates(db):
    from data_flow_control import Policy, Resolution
    from data_flow_control.intermediate_policy import compile_policy_intermediates

    for constraint in (
        "COUNT(DISTINCT cases.category) > 4",
        "ARBITRARY(cases.category) = 'x'",
        "SUM(cases.amount + other.amount) > 0",
    ):
        with pytest.raises(IntermediateError):
            compile_policy_intermediates(
                db,
                Policy(
                    constraint=constraint, sources=["cases", "other"], on_fail=Resolution.REMOVE
                ),
            )


@pytest.mark.parametrize("aggregate", ["bool_and", "bool_or"])
def test_null_only_boolean_state_merge(db, aggregate):
    plan = PolicyIntermediates(db, [Intermediate("b", "cases", "priority = 'High'", aggregate)])
    state = capture(plan, "id='D'")
    assert plan.values(plan.merge(state, state)) == {"b": None}


def test_intermediate_names_cannot_shadow_cardinality(db):
    with pytest.raises(IntermediateError, match="cardinality"):
        PolicyIntermediates(
            db,
            [
                Intermediate("id", "cases", "id"),
                Intermediate("id__cardinality", "cases", "amount", "sum"),
            ],
        )


def test_same_identity_set_changed_snapshot_rejected(db):
    plan = PolicyIntermediates(db, [Intermediate("total", "cases", "amount", "sum")])
    before = capture(plan, "id='A'")
    db.execute("UPDATE cases SET amount=100 WHERE id='A'")
    with pytest.raises(IntermediateError, match="snapshot"):
        plan.merge(before, capture(plan, "id='A'"))


def test_negated_implicit_uniqueness_rejected(db):
    from data_flow_control import Policy
    from data_flow_control.intermediate_policy import compile_policy_intermediates

    with pytest.raises(IntermediateError, match="Negated"):
        compile_policy_intermediates(
            db, Policy.from_pgn("SOURCE cases CONSTRAINT NOT (cases.amount > 1) ON FAIL REMOVE")
        )


def test_nanosecond_timestamp_identity_fails_before_python_truncation(db):
    db.execute("CREATE TABLE precise(ts TIMESTAMP_NS, amount INTEGER)")
    db.execute("""INSERT INTO precise VALUES
        ('2026-01-01 00:00:00.000000001',10), ('2026-01-01 00:00:00.000000002',10)""")
    plan = PolicyIntermediates(db, [Intermediate("total", "precise", "amount", "sum")])
    for suffix in (
        "",
        " WHERE ts='2026-01-01 00:00:00.000000001'::TIMESTAMP_NS",
        " WHERE ts='2026-01-01 00:00:00.000000002'::TIMESTAMP_NS",
    ):
        with pytest.raises(IntermediateError, match="TIMESTAMP_NS"):
            plan.capture("precise", "SELECT * FROM precise" + suffix, identity_columns=["ts"])


def test_nanosecond_timestamp_unique_values_fail_before_python_truncation(db):
    db.execute("CREATE TABLE precise(id INTEGER, ts TIMESTAMP_NS)")
    db.execute("""INSERT INTO precise VALUES
        (1,'2026-01-01 00:00:00.000000001'), (2,'2026-01-01 00:00:00.000000002')""")
    plan = PolicyIntermediates(db, [Intermediate("stamp", "precise", "ts")])
    for suffix in ("", " WHERE id=1", " WHERE id=2"):
        with pytest.raises(IntermediateError, match="TIMESTAMP_NS"):
            plan.capture("precise", "SELECT * FROM precise" + suffix, identity_columns=["id"])


def test_explicit_lossless_sql_timestamp_encoding_keeps_distinct_contributors(db):
    db.execute("CREATE TABLE precise(ts TIMESTAMP_NS, amount INTEGER)")
    db.execute("""INSERT INTO precise VALUES
        ('2026-01-01 00:00:00.000000001',10), ('2026-01-01 00:00:00.000000002',10)""")
    plan = PolicyIntermediates(
        db,
        [
            Intermediate("total", "precise", "amount", "sum"),
            Intermediate("stamp", "precise", "CAST(ts AS VARCHAR)"),
        ],
    )
    query = "SELECT *, CAST(ts AS VARCHAR) AS identity FROM precise"
    states = [
        plan.capture(
            "precise", query + f" ORDER BY ts LIMIT 1 OFFSET {n}", identity_columns=["identity"]
        )
        for n in range(2)
    ]
    assert plan.values(plan.merge(*states)) == {"total": 20, "stamp": (2, None)}
    assert not plan.evaluate(plan.merge(*states), "total <= 10")


def test_native_state_transport_preserves_embedded_nul_unicode_and_binary(db):
    text = "nul\0quote'雪"
    db.execute("UPDATE cases SET category=? WHERE id IN ('A','B')", [text])
    plan = PolicyIntermediates(db, [Intermediate("cat", "cases", "category")])
    state = plan.merge(capture(plan, "id='A'"), capture(plan, "id='B'"))
    assert plan.values(state) == {"cat": (1, text)}
    assert plan.evaluate(state, "cat__cardinality=1 AND cat=expected", sink={"expected": text})
    db.execute("CREATE TABLE binary_input(id INT, payload BLOB)")
    db.execute("INSERT INTO binary_input VALUES (1, ?)", [b"\x00\xff"])
    binary = PolicyIntermediates(db, [Intermediate("payload", "binary_input", "payload")])
    blob = binary.capture("binary_input", "SELECT * FROM binary_input", identity_columns=["id"])
    assert binary.values(binary.merge(blob, blob)) == {"payload": (1, b"\x00\xff")}


def test_native_identity_type_change_cannot_double_count_same_key(db):
    db.execute("CREATE TABLE typed_input(id INT, amount INT)")
    db.execute("INSERT INTO typed_input VALUES (1,10)")
    plan = PolicyIntermediates(db, [Intermediate("total", "typed_input", "amount", "sum")])
    a = plan.capture("typed_input", "SELECT * FROM typed_input", identity_columns=["id"])
    b = plan.capture(
        "typed_input",
        "SELECT CAST(id AS BIGINT) AS id, amount FROM typed_input",
        identity_columns=["id"],
    )
    with pytest.raises(IntermediateError, match="identity schema"):
        plan.merge(a, b)
