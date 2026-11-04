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
