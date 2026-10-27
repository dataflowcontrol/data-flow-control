"""0.2.1 literal-KILL and NULL-test output binding runtime regressions."""

from __future__ import annotations

import sqlite3

import duckdb
import pytest

from data_flow_control import Policy, Resolution, dfc
from data_flow_control.adapters.kill import kill_exc_match

READ_ENGINES = [
    "duckdb",
    "sqlite",
    "datafusion",
    pytest.param("postgres", marks=pytest.mark.postgres),
]
WRITE_ENGINES = ["duckdb", "sqlite", pytest.param("postgres", marks=pytest.mark.postgres)]


@pytest.fixture
def engine(request):
    name = request.param
    if name == "datafusion":
        datafusion = pytest.importorskip("datafusion")
        import pyarrow as pa

        raw = datafusion.SessionContext()
        raw.register_record_batches(
            "dfc021_a", [pa.table({"x": pa.array([1, None], pa.int64())}).to_batches()]
        )
        raw.register_record_batches("dfc021_b", [pa.table({"x": [2]}).to_batches()])
    else:
        if name == "duckdb":
            raw = duckdb.connect()
        elif name == "sqlite":
            raw = sqlite3.connect(":memory:")
        else:
            psycopg = pytest.importorskip("psycopg")
            services = request.getfixturevalue("passant_docker")
            raw = psycopg.connect(services.postgres_url, autocommit=True)
        for sql in [
            "CREATE TABLE dfc021_a(x INTEGER)",
            "INSERT INTO dfc021_a VALUES (1), (NULL)",
            "CREATE TABLE dfc021_b(x INTEGER)",
            "INSERT INTO dfc021_b VALUES (2)",
            "CREATE TABLE dfc021_target(x INTEGER, y INTEGER)",
        ]:
            raw.execute(sql)
    db = dfc(raw, dialect=name)
    try:
        yield db
    finally:
        if name == "postgres":
            for table in ["dfc021_target", "dfc021_b", "dfc021_a"]:
                raw.execute(f"DROP TABLE {table}")
        db.close()


def kill_policy(db, constraint, **kwargs):
    db.register_policy(
        Policy(sources=["dfc021_a"], constraint=constraint, on_fail=Resolution.KILL, **kwargs)
    )


@pytest.mark.parametrize("engine", READ_ENGINES, indirect=True)
def test_literal_false_kill_aborts_nonempty(engine):
    kill_policy(engine, "FALSE")
    with pytest.raises(Exception, match=kill_exc_match(engine.adapter.dialect)):
        engine.fetchall("SELECT x FROM dfc021_a")


@pytest.mark.parametrize("engine", READ_ENGINES, indirect=True)
def test_literal_false_kill_does_not_abort_genuinely_empty_input(engine):
    kill_policy(engine, "FALSE")
    assert engine.fetchall("SELECT x FROM dfc021_a WHERE 1=0") == []


@pytest.mark.parametrize("engine", READ_ENGINES, indirect=True)
@pytest.mark.parametrize("where,expected", [("", [(1,)]), ("WHERE x IS NULL", [])])
def test_kill_null_keeps_existing_filter_semantics(engine, where, expected):
    kill_policy(engine, "dfc021_a.x > 0")
    assert engine.fetchall(f"SELECT x FROM dfc021_a {where}") == expected


@pytest.mark.parametrize("engine", WRITE_ENGINES, indirect=True)
@pytest.mark.parametrize("required", [False, True])
def test_literal_abort_insert_is_atomic_including_missing_required_source(engine, required):
    engine.adapter.execute("INSERT INTO dfc021_target VALUES (99, 99)")
    kill_policy(
        engine,
        "TRUE" if required else "FALSE",
        sink="dfc021_target",
        required_sources=["dfc021_a"] if required else [],
    )
    source = "dfc021_b" if required else "dfc021_a"
    with pytest.raises(Exception, match=kill_exc_match(engine.adapter.dialect)):
        engine.execute(f"INSERT INTO dfc021_target SELECT x AS x, x AS y FROM {source}")
    assert engine.adapter.execute("SELECT * FROM dfc021_target").fetchall() == [(99, 99)]


@pytest.mark.parametrize("engine", READ_ENGINES, indirect=True)
@pytest.mark.parametrize("test,expected", [("IS NULL", [(None,)]), ("IS NOT NULL", [(1,)])])
def test_output_null_tests_map_no_sink_projected_alias(engine, test, expected):
    engine.register_policy(
        Policy(
            sources=["dfc021_a"], constraint=f"_OUTPUT_.renamed {test}", on_fail=Resolution.REMOVE
        )
    )
    assert engine.fetchall("SELECT x AS renamed FROM dfc021_a") == expected


@pytest.mark.parametrize("engine", READ_ENGINES, indirect=True)
@pytest.mark.parametrize("test", ["IS NULL", "IS NOT NULL"])
def test_output_null_tests_missing_public_column_skip_policy(engine, test):
    kill_policy(engine, f"_OUTPUT_.missing {test} AND FALSE")
    assert engine.fetchall("SELECT x FROM dfc021_a ORDER BY x NULLS LAST") == [(1,), (None,)]


@pytest.mark.parametrize("engine", WRITE_ENGINES, indirect=True)
@pytest.mark.parametrize("reference", ["_OUTPUT_.x", "dest.x"])
@pytest.mark.parametrize("test,expected", [("IS NULL", [(None, 7)]), ("IS NOT NULL", [(1, 7)])])
def test_output_null_tests_map_reordered_insert_and_sink_alias(engine, reference, test, expected):
    engine.register_policy(
        Policy(
            sources=["dfc021_a"],
            sink="dfc021_target",
            sink_alias="dest",
            constraint=f"{reference} {test}",
            on_fail=Resolution.REMOVE,
        )
    )
    engine.execute(
        "INSERT INTO dfc021_target(y,x) SELECT 7 AS private_y, x AS private_x FROM dfc021_a"
    )
    assert engine.adapter.execute("SELECT x,y FROM dfc021_target").fetchall() == expected


@pytest.mark.parametrize("engine", WRITE_ENGINES, indirect=True)
@pytest.mark.parametrize("required", [False, True])
def test_literal_select_without_from_aborts_sink_write(engine, required):
    engine.adapter.execute("INSERT INTO dfc021_target VALUES (99, 99)")
    engine.register_policy(
        Policy(
            sources=["dfc021_a"] if required else [],
            required_sources=["dfc021_a"] if required else [],
            sink="dfc021_target",
            constraint="TRUE" if required else "FALSE",
            on_fail=Resolution.KILL,
        )
    )
    with pytest.raises(Exception, match=kill_exc_match(engine.adapter.dialect)):
        engine.execute("INSERT INTO dfc021_target SELECT 1, 1")
    assert engine.adapter.execute("SELECT * FROM dfc021_target").fetchall() == [(99, 99)]


@pytest.mark.parametrize("engine", READ_ENGINES, indirect=True)
@pytest.mark.parametrize("test,fails", [("IS NULL", True), ("IS NOT NULL", False)])
def test_retry_output_null_predicate_on_nonnull_json_string(engine, test, fails):
    from data_flow_control import RetryViolation

    engine.register_policy(
        Policy(
            sources=["dfc021_a"],
            constraint=f"_OUTPUT_.payload {test}",
            on_fail=Resolution.RETRY,
            retry_message="exact payload feedback",
        )
    )
    query = "SELECT '{\"k\":1}' AS payload FROM dfc021_a WHERE x > 0"
    if fails:
        with pytest.raises(RetryViolation) as exc:
            engine.execute(query)
        assert exc.value.messages == ["exact payload feedback"]
    else:
        assert engine.fetchall(query) == [('{"k":1}',)]
