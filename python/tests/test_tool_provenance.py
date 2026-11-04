"""SQL tool models use the ordinary DFC rewrite path, without executing tools."""

import duckdb
import pytest

from data_flow_control import Policy, Resolution, ToolOutput, ToolProvenance, dfc


@pytest.fixture
def db():
    connection = dfc(duckdb.connect())
    connection.execute("CREATE TABLE ToolInput (id INTEGER, category VARCHAR, copies INTEGER)")
    connection.execute("INSERT INTO ToolInput VALUES (1, 'public', 2), (2, 'private', 1)")
    connection.execute("CREATE TABLE ToolOutput (message VARCHAR)")
    yield connection
    connection.close()


def rows(db):
    return db.raw_connection.execute("SELECT message FROM ToolOutput ORDER BY message").fetchall()


def test_select_star_one_to_one(db):
    model = ToolProvenance("SELECT * FROM ToolInput WHERE id = ?")
    assert (
        model.insert_outputs(db, "ToolOutput", [ToolOutput((), {"message": "sent"})], params=[1])
        == 1
    )
    assert rows(db) == [("sent",)]


def test_one_to_many_expansion_with_duplicate_payloads_and_reordered_keys(db):
    model = ToolProvenance(
        "SELECT i.*, n.copy FROM ToolInput i, LATERAL generate_series(1, i.copies) n(copy)",
        ("id", "copy"),
    )
    assert (
        model.insert_outputs(
            db,
            "ToolOutput",
            [
                ToolOutput((2, 1), {"message": "same"}),
                ToolOutput((1, 2), {"message": "same"}),
                ToolOutput((1, 1), {"message": "same"}),
            ],
        )
        == 3
    )
    assert rows(db) == [("same",)] * 3


@pytest.mark.parametrize(
    "sql,keys",
    [
        ("SELECT a.id AS a, b.id AS b FROM ToolInput a JOIN ToolInput b ON a.id < b.id", [(1, 2)]),
        (
            "SELECT category AS a, SUM(copies) AS b FROM ToolInput GROUP BY category",
            [("public", 2), ("private", 1)],
        ),
        ("SELECT id AS a, SUM(copies) OVER (ORDER BY id) AS b FROM ToolInput", [(1, 2), (2, 3)]),
        (
            "WITH picked AS (SELECT * FROM ToolInput WHERE id = 1) SELECT id AS a, copies AS b FROM picked",
            [(1, 2)],
        ),
    ],
)
def test_relational_models(db, sql, keys):
    model = ToolProvenance(sql, ("a", "b"))
    assert model.insert_outputs(
        db, "ToolOutput", [ToolOutput(key, {"message": str(key)}) for key in keys]
    ) == len(keys)


def test_external_source_join(db):
    db.execute("CREATE TABLE ExternalSnapshot (id INTEGER, label VARCHAR)")
    db.execute("INSERT INTO ExternalSnapshot VALUES (1, 'restricted'), (2, 'public')")
    db.register_policy(
        Policy(
            sources=["ExternalSnapshot"],
            sink="ToolOutput",
            constraint="ExternalSnapshot.label = 'public'",
            on_fail=Resolution.KILL,
        )
    )
    model = ToolProvenance(
        "SELECT i.id, e.label FROM ToolInput i JOIN ExternalSnapshot e ON i.id = e.id", ("id",)
    )
    with pytest.raises(Exception, match="KILLing"):
        model.insert_outputs(
            db,
            "ToolOutput",
            [
                ToolOutput((1,), {"message": "one"}),
                ToolOutput((2,), {"message": "two"}),
            ],
        )
    assert rows(db) == []


@pytest.mark.parametrize("resolution", [Resolution.KILL, Resolution.REMOVE])
def test_model_reaches_policy_rewriter(db, resolution):
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = 'public'",
            on_fail=resolution,
        )
    )
    model = ToolProvenance("SELECT * FROM ToolInput", ("id",))
    outputs = [ToolOutput((2,), {"message": "private"}), ToolOutput((1,), {"message": "public"})]
    if resolution == Resolution.KILL:
        with pytest.raises(Exception, match="KILLing"):
            model.insert_outputs(db, "ToolOutput", outputs)
        assert rows(db) == []
    else:
        assert model.insert_outputs(db, "ToolOutput", outputs) == 1
        assert rows(db) == [("public",)]


@pytest.mark.parametrize(
    "sql,outputs,match",
    [
        ("SELECT * FROM ToolInput", [ToolOutput((1,), {"message": "x"})], "match all"),
        ("SELECT * FROM ToolInput WHERE id = 1", [ToolOutput((2,), {"message": "x"})], "match all"),
        (
            "SELECT * FROM ToolInput WHERE id = 1",
            [ToolOutput((1,), {"message": "x"})] * 2,
            "unique",
        ),
        ("SELECT 1 AS id FROM ToolInput", [ToolOutput((1,), {"message": "x"})], "unique"),
        ("SELECT NULL AS id", [ToolOutput((None,), {"message": "x"})], "NULL"),
    ],
)
def test_invalid_correspondence_does_not_publish(db, sql, outputs, match):
    with pytest.raises(ValueError, match=match):
        ToolProvenance(sql, ("id",)).insert_outputs(db, "ToolOutput", outputs)
    assert rows(db) == []


def test_empty_model_and_outputs(db):
    assert (
        ToolProvenance("SELECT * FROM ToolInput WHERE FALSE").insert_outputs(db, "ToolOutput", [])
        == 0
    )
    with pytest.raises(ValueError, match="match all"):
        ToolProvenance("SELECT * FROM ToolInput WHERE id = 1").insert_outputs(db, "ToolOutput", [])


def test_singleton_rejects_multiple_modeled_occurrences(db):
    with pytest.raises(ValueError, match="unique"):
        ToolProvenance("SELECT * FROM ToolInput").insert_outputs(
            db, "ToolOutput", [ToolOutput((), {"message": "x"})]
        )


def test_failure_does_not_rollback_callers_transaction(db):
    db.raw_connection.execute("BEGIN")
    db.raw_connection.execute("INSERT INTO ToolOutput VALUES ('prior')")
    with pytest.raises(Exception, match="transaction"):
        ToolProvenance("SELECT * FROM ToolInput WHERE id=1").insert_outputs(
            db, "ToolOutput", [ToolOutput((), {"message": "new"})]
        )
    # DuckDB marks the caller's transaction aborted on a nested BEGIN. The
    # publication helper must not commit it; caller retains rollback ownership.
    db.raw_connection.execute("ROLLBACK")
    assert rows(db) == []


def test_bad_actual_payload_rolls_back_and_connection_remains_usable(db):
    model = ToolProvenance("SELECT * FROM ToolInput WHERE id = 1")
    with pytest.raises(Exception, match="column"):
        model.insert_outputs(db, "ToolOutput", [ToolOutput((), {"missing": "x"})])
    assert rows(db) == []
    assert model.insert_outputs(db, "ToolOutput", [ToolOutput((), {"message": "ok"})]) == 1


@pytest.mark.parametrize(
    "sql", ["DELETE FROM ToolInput", "SELECT 1; SELECT 2", "SELECT random()", "SELECT uuid()"]
)
def test_reject_nonqueries_and_known_volatile_functions(sql):
    with pytest.raises(ValueError):
        ToolProvenance(sql)


@pytest.mark.parametrize("limit,passes", [(1, True), (2, False)])
def test_grouped_model_enforces_aggregate_policy(db, limit, passes):
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint=f"COUNT(ToolInput.id) = {limit}",
            on_fail=Resolution.KILL,
        )
    )
    model = ToolProvenance(
        "SELECT category, COUNT(*) AS n FROM ToolInput GROUP BY category", ("category",)
    )
    outputs = [ToolOutput((category,), {"message": category}) for category in ("public", "private")]
    if passes:
        assert model.insert_outputs(db, "ToolOutput", outputs) == 2
    else:
        with pytest.raises(Exception, match="KILLing"):
            model.insert_outputs(db, "ToolOutput", outputs)
        assert rows(db) == []


def test_self_join_model_enforces_policy(db):
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = 'public'",
            on_fail=Resolution.KILL,
        )
    )
    model = ToolProvenance(
        "SELECT a.id AS a, b.id AS b FROM ToolInput a JOIN ToolInput b ON a.id < b.id", ("a", "b")
    )
    with pytest.raises(Exception, match="KILLing"):
        model.insert_outputs(db, "ToolOutput", [ToolOutput((1, 2), {"message": "pair"})])
    assert rows(db) == []


def test_window_policy_checks_contributor_frame_and_rolls_back(db):
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = 'public'",
            on_fail=Resolution.KILL,
        )
    )
    model = ToolProvenance("SELECT id, SUM(copies) OVER (ORDER BY id) AS n FROM ToolInput", ("id",))
    # The second cumulative frame contains a private contributor.
    with pytest.raises(Exception, match="KILLing"):
        model.insert_outputs(
            db,
            "ToolOutput",
            [
                ToolOutput((1,), {"message": "one"}),
                ToolOutput((2,), {"message": "two"}),
            ],
        )
    assert rows(db) == []


def test_sql_collation_cannot_duplicate_occurrences(db):
    db.execute("CREATE TABLE CollatedInput (id VARCHAR COLLATE NOCASE)")
    db.execute("INSERT INTO CollatedInput VALUES ('a'), ('A')")
    model = ToolProvenance("SELECT * FROM CollatedInput", ("id",))
    with pytest.raises(ValueError, match="SQL equality"):
        model.insert_outputs(
            db,
            "ToolOutput",
            [
                ToolOutput(("a",), {"message": "lower"}),
                ToolOutput(("A",), {"message": "upper"}),
            ],
        )
    assert rows(db) == []


@pytest.mark.parametrize("swapped", [False, True])
def test_actual_output_values_remain_correlated_with_model_keys(db, swapped):
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = ToolOutput.message",
            on_fail=Resolution.KILL,
        )
    )
    model = ToolProvenance("SELECT * FROM ToolInput WHERE id >= ?", ("id",))
    outputs = [
        ToolOutput((2,), {"message": "public" if swapped else "private"}),
        ToolOutput((1,), {"message": "private" if swapped else "public"}),
    ]
    if swapped:
        with pytest.raises(Exception, match="KILLing"):
            model.insert_outputs(db, "ToolOutput", outputs, params=[1])
        assert rows(db) == []
    else:
        assert model.insert_outputs(db, "ToolOutput", outputs, params=[1]) == 2
        assert rows(db) == [("private",), ("public",)]


def test_question_mark_literals_do_not_shift_parameters(db):
    model = ToolProvenance("SELECT * FROM ToolInput WHERE category != '?' AND id = ?")
    assert (
        model.insert_outputs(db, "ToolOutput", [ToolOutput((), {"message": "?"})], params=[1]) == 1
    )
    assert rows(db) == [("?",)]


@pytest.mark.parametrize("swapped", [False, True])
def test_one_to_many_input_output_policy(db, swapped):
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = ToolOutput.message",
            on_fail=Resolution.KILL,
        )
    )
    model = ToolProvenance(
        "SELECT i.*, n.copy FROM ToolInput i CROSS JOIN (VALUES (1), (2)) n(copy) "
        "WHERE n.copy <= i.copies",
        ("id", "copy"),
    )
    outputs = [
        ToolOutput((2, 1), {"message": "public" if swapped else "private"}),
        ToolOutput((1, 2), {"message": "public"}),
        ToolOutput((1, 1), {"message": "public"}),
    ]
    if swapped:
        with pytest.raises(Exception, match="KILLing"):
            model.insert_outputs(db, "ToolOutput", outputs)
        assert rows(db) == []
    else:
        assert model.insert_outputs(db, "ToolOutput", outputs) == 3


def test_lateral_expansion_enforces_policy_and_rolls_back(db):
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = 'public'",
            on_fail=Resolution.KILL,
        )
    )
    model = ToolProvenance(
        "SELECT i.*, n.copy FROM ToolInput i, LATERAL generate_series(1, i.copies) n(copy)",
        ("id", "copy"),
    )
    with pytest.raises(Exception, match="KILLing"):
        model.insert_outputs(
            db,
            "ToolOutput",
            [
                ToolOutput((2, 1), {"message": "private"}),
                ToolOutput((1, 2), {"message": "public"}),
                ToolOutput((1, 1), {"message": "public"}),
            ],
        )
    assert rows(db) == []


def test_lateral_expansion_with_passing_input_output_policy(db):
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = ToolOutput.message",
            on_fail=Resolution.KILL,
        )
    )
    model = ToolProvenance(
        "SELECT i.*, n.copy FROM ToolInput i, LATERAL generate_series(1, i.copies) n(copy) "
        "WHERE i.id = ?",
        ("id", "copy"),
    )
    assert (
        model.insert_outputs(
            db,
            "ToolOutput",
            [
                ToolOutput((1, 2), {"message": "public"}),
                ToolOutput((1, 1), {"message": "public"}),
            ],
            params=[1],
        )
        == 2
    )


@pytest.mark.parametrize(
    "function",
    [
        "LEAD(category) OVER (ORDER BY id)",
        "FIRST_VALUE(category) OVER (ORDER BY id DESC)",
        "FILL(copies) OVER (ORDER BY id)",
    ],
)
@pytest.mark.parametrize("resolution", [Resolution.REMOVE, Resolution.KILL])
def test_navigation_window_cannot_publish_other_rows_private_value(db, function, resolution):
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = 'public'",
            on_fail=resolution,
        )
    )
    model = ToolProvenance(f"SELECT id, {function} AS next_value FROM ToolInput", ("id",))
    # A WHERE injected before the window would retain key 1 while dropping its
    # private contributor. Actual external payloads cannot be recomputed by SQL.
    with pytest.raises(Exception, match="window"):
        model.insert_outputs(
            db,
            "ToolOutput",
            [
                ToolOutput((1,), {"message": "private"}),
                ToolOutput((2,), {"message": "none"}),
            ],
        )
    assert rows(db) == []


@pytest.mark.parametrize("source", ["ToolInput", "Secrets"])
@pytest.mark.parametrize("resolution", [Resolution.REMOVE, Resolution.KILL])
def test_scalar_subquery_payload_dependencies_cannot_disappear(db, source, resolution):
    db.execute("CREATE TABLE Secrets AS SELECT * FROM ToolInput")
    db.register_policy(
        Policy(
            sources=[source],
            sink="ToolOutput",
            constraint=f"{source}.category = 'public'",
            on_fail=resolution,
        )
    )
    model = ToolProvenance(
        f"SELECT id, (SELECT category FROM {source} WHERE id=2) AS value FROM ToolInput", ("id",)
    )
    with pytest.raises(ValueError, match="scalar/expression subqueries"):
        model.insert_outputs(
            db,
            "ToolOutput",
            [
                ToolOutput((1,), {"message": "private"}),
                ToolOutput((2,), {"message": "private"}),
            ],
        )
    assert rows(db) == []


@pytest.mark.parametrize(
    "query,key",
    [
        ("SELECT string_agg(category) AS message FROM ToolInput", ()),
        ("SELECT 1 AS id, string_agg(category) AS message FROM ToolInput GROUP BY 1", (1,)),
        ("SELECT DISTINCT 1 AS id FROM ToolInput", (1,)),
        ("SELECT 1 AS id FROM ToolInput UNION SELECT 1 AS id FROM ToolInput", (1,)),
        ("SELECT a.id FROM ToolInput a LEFT JOIN ToolInput b ON a.id < b.id WHERE a.id=1", (1,)),
    ],
)
def test_remove_cannot_recompute_contributors_of_fixed_payload(db, query, key):
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = 'public'",
            on_fail=Resolution.REMOVE,
        )
    )
    model = ToolProvenance(query, ("id",) if key else ())
    outputs = [ToolOutput(key, {"message": "public,private"})]
    if "string_agg" in query:
        assert model.insert_outputs(db, "ToolOutput", outputs) == 0
    else:
        with pytest.raises(ValueError, match="row-preserving"):
            model.insert_outputs(db, "ToolOutput", outputs)
    assert rows(db) == []


def test_window_model_preserves_original_frame_values_when_allowed(db):
    db.raw_connection.execute("UPDATE ToolInput SET category='public'")
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = 'public'",
            on_fail=Resolution.KILL,
        )
    )
    model = ToolProvenance(
        "SELECT id, SUM(copies) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING "
        "AND CURRENT ROW) AS total FROM ToolInput",
        ("id", "total"),
    )
    assert (
        model.insert_outputs(
            db,
            "ToolOutput",
            [
                ToolOutput((2, 3), {"message": "three"}),
                ToolOutput((1, 2), {"message": "two"}),
            ],
        )
        == 2
    )
    assert rows(db) == [("three",), ("two",)]


def test_array_subquery_payload_dependency_is_not_discarded(db):
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = 'public'",
            on_fail=Resolution.REMOVE,
        )
    )
    model = ToolProvenance(
        "SELECT id, ARRAY(SELECT category FROM ToolInput) AS value FROM ToolInput", ("id",)
    )
    with pytest.raises(ValueError, match="scalar/expression subqueries"):
        model.insert_outputs(
            db,
            "ToolOutput",
            [
                ToolOutput((1,), {"message": "public,private"}),
                ToolOutput((2,), {"message": "public,private"}),
            ],
        )
    assert rows(db) == []


def test_interpolation_window_is_rejected_even_when_current_rows_pass(db):
    db.raw_connection.execute("UPDATE ToolInput SET category='public'")
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = 'public'",
            on_fail=Resolution.KILL,
        )
    )
    model = ToolProvenance(
        "SELECT id, FILL(copies) OVER (ORDER BY id) AS value FROM ToolInput", ("id",)
    )
    with pytest.raises(Exception, match="window"):
        model.insert_outputs(
            db,
            "ToolOutput",
            [
                ToolOutput((1,), {"message": "one"}),
                ToolOutput((2,), {"message": "two"}),
            ],
        )
    assert rows(db) == []


def test_intermediate_binding_requires_propagated_states(db):
    model = ToolProvenance("SELECT * FROM ToolInput", ("id",))
    result = db.execute(model.sql)
    with pytest.raises(ValueError, match="automatic intermediate states"):
        model.bind_intermediates(result, [ToolOutput((1,), {"message": "x"})])
    assert rows(db) == []


@pytest.mark.parametrize("grouped", [False, True])
def test_remove_discards_complete_original_aggregate(db, grouped):
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = 'public'",
            on_fail=Resolution.REMOVE,
        )
    )
    model = ToolProvenance(
        "SELECT "
        + ("copies, " if grouped else "")
        + "string_agg(category) AS contents FROM ToolInput"
        + (" GROUP BY copies" if grouped else ""),
        ("copies",) if grouped else (),
    )
    outputs = (
        [ToolOutput((2,), {"message": "public"}), ToolOutput((1,), {"message": "private"})]
        if grouped
        else [ToolOutput((), {"message": "public,private"})]
    )
    assert model.insert_outputs(db, "ToolOutput", outputs) == int(grouped)
    assert rows(db) == ([("public",)] if grouped else [])
    assert db.policies()[0].on_fail is Resolution.REMOVE


def test_remove_window_discards_complete_original_frames(db):
    db.execute("INSERT INTO ToolInput VALUES (3, 'public', 4), (4, 'public', 8)")
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = 'public'",
            on_fail=Resolution.REMOVE,
        )
    )
    model = ToolProvenance(
        "SELECT id, SUM(copies) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS total FROM ToolInput",
        ("id",),
    )
    outputs = [
        ToolOutput((i,), {"message": str(total)}) for i, total in [(1, 2), (2, 3), (3, 5), (4, 12)]
    ]
    assert model.insert_outputs(db, "ToolOutput", outputs) == 2
    assert rows(db) == [("12",), ("2",)]


def test_remove_self_join_checks_both_contributors(db):
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = 'public'",
            on_fail=Resolution.REMOVE,
        )
    )
    model = ToolProvenance(
        "SELECT a.id AS a, b.id AS b FROM ToolInput a CROSS JOIN ToolInput b", ("a", "b")
    )
    outputs = [ToolOutput((a, b), {"message": f"{a},{b}"}) for a in (1, 2) for b in (1, 2)]
    assert model.insert_outputs(db, "ToolOutput", outputs) == 1
    assert rows(db) == [("1,1",)]


def test_whole_output_remove_preserves_joint_window_predicates(db):
    for constraint in ("SUM(ToolInput.copies) >= 3", "SUM(ToolInput.copies) <= 3"):
        db.register_policy(
            Policy(
                sources=["ToolInput"],
                sink="ToolOutput",
                constraint=constraint,
                on_fail=Resolution.REMOVE,
            )
        )
    model = ToolProvenance(
        "SELECT id, SUM(copies) OVER (ORDER BY id) AS total FROM ToolInput", ("id",)
    )
    assert (
        model.insert_outputs(
            db,
            "ToolOutput",
            [ToolOutput((1,), {"message": "2"}), ToolOutput((2,), {"message": "3"})],
        )
        == 1
    )
    assert rows(db) == [("3",)]


def test_whole_output_remove_checks_actual_sink_and_restores_planner(db):
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="COUNT(ToolInput.id) = 2",
            on_fail=Resolution.REMOVE,
        )
    )
    db.register_policy(
        Policy(
            sources=[],
            sink="ToolOutput",
            constraint="ToolOutput.message = 'allowed'",
            on_fail=Resolution.KILL,
        )
    )
    original = db.planner
    model = ToolProvenance("SELECT SUM(copies) AS total FROM ToolInput")
    with pytest.raises(Exception, match="KILLing"):
        model.insert_outputs(db, "ToolOutput", [ToolOutput((), {"message": "denied"})])
    assert db.planner is original
    assert rows(db) == []
    assert model.insert_outputs(db, "ToolOutput", [ToolOutput((), {"message": "allowed"})]) == 1


def test_whole_output_remove_rejects_null_contributor(db):
    db.raw_connection.execute("UPDATE ToolInput SET category=NULL WHERE id=2")
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = 'public'",
            on_fail=Resolution.REMOVE,
        )
    )
    model = ToolProvenance("SELECT COUNT(*) AS n FROM ToolInput")
    assert (
        model.insert_outputs(db, "ToolOutput", [ToolOutput((), {"message": "contains null row"})])
        == 0
    )
    assert rows(db) == []


def test_whole_output_remove_rejects_nested_aggregation(db):
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = 'public'",
            on_fail=Resolution.REMOVE,
        )
    )
    model = ToolProvenance(
        "SELECT SUM(n) AS total FROM (SELECT category, COUNT(*) AS n FROM ToolInput GROUP BY category) groups"
    )
    with pytest.raises(ValueError, match="single flat"):
        model.insert_outputs(
            db, "ToolOutput", [ToolOutput((), {"message": "contains private group"})]
        )
    assert rows(db) == []


def test_aggregate_input_output_comparison_fails_closed_until_deferred_binding(db):
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="ToolInput.category = ToolOutput.message",
            on_fail=Resolution.REMOVE,
        )
    )
    model = ToolProvenance(
        "SELECT category, COUNT(*) AS n FROM ToolInput GROUP BY category", ("category",)
    )
    with pytest.raises(Exception, match="ToolOutput|unsupported"):
        model.insert_outputs(
            db,
            "ToolOutput",
            [
                ToolOutput(("public",), {"message": "private"}),
                ToolOutput(("private",), {"message": "public"}),
            ],
        )
    assert rows(db) == []


def test_source_remove_cannot_hide_original_actual_sink_kill(db):
    db.register_policy(
        Policy(
            sources=["ToolInput"],
            sink="ToolOutput",
            constraint="COUNT(ToolInput.id) = 99",
            on_fail=Resolution.REMOVE,
        )
    )
    db.register_policy(
        Policy(
            sources=[],
            sink="ToolOutput",
            constraint="ToolOutput.message = 'allowed'",
            on_fail=Resolution.KILL,
        )
    )
    model = ToolProvenance("SELECT COUNT(*) AS n FROM ToolInput")
    with pytest.raises(Exception, match="KILLing"):
        model.insert_outputs(db, "ToolOutput", [ToolOutput((), {"message": "denied"})])
    assert rows(db) == []
    assert model.insert_outputs(db, "ToolOutput", [ToolOutput((), {"message": "allowed"})]) == 0
