import duckdb
import pytest

from data_flow_control import Policy, dfc
from data_flow_control.intermediates import IntermediateError


@pytest.fixture
def conn():
    with dfc(duckdb.connect()) as c:
        c.execute("CREATE TABLE cases(id VARCHAR, category VARCHAR, priority VARCHAR, amount INT)")
        c.execute(
            "INSERT INTO cases VALUES ('A','Benefits','Low',10), ('B','Discrimination','High',20), ('C','Discrimination','Low',30)"
        )
        c.execute("CREATE TABLE updates(case_id VARCHAR, success BOOLEAN)")
        c.register_policy(
            Policy.from_pgn("""SOURCE REQUIRED cases AS c SINK updates AS u
            CONSTRAINT c.id = u.case_id AND BOOL_AND(c.category <> 'Discrimination' OR c.priority = 'High')
            AND u.success ON FAIL REMOVE""")
        )
        c.enable_intermediate_propagation()
        yield c


def test_normal_execute_carries_case_b_across_materialization(conn):
    conn.execute(
        "CREATE TEMP TABLE send_input AS SELECT id, 'body' AS body FROM cases WHERE id='B'"
    )
    conn.execute("CREATE TABLE send_output AS SELECT id, TRUE AS success FROM send_input")
    assert conn.fetchall("SELECT * FROM send_output") == [("B", True)]
    assert conn.fetchall("INSERT INTO updates SELECT 'A', success FROM send_output") == [(0,)]
    assert conn.fetchall("INSERT INTO updates SELECT id, success FROM send_output") == [(1,)]
    assert conn.fetchall("SELECT * FROM updates") == [("B", True)]


def test_grouped_joint_expression_and_conflicting_case_identity(conn):
    conn.execute(
        "CREATE TABLE combined AS SELECT 'B' AS id, COUNT(*) AS n FROM cases WHERE id IN ('A','B')"
    )
    assert conn.fetchall("INSERT INTO updates SELECT id, TRUE FROM combined") == [(0,)]
    conn.execute("CREATE TABLE wrong AS SELECT id FROM cases WHERE id='C'")
    assert conn.fetchall("INSERT INTO updates SELECT id, TRUE FROM wrong") == [(0,)]


def test_fanout_self_join_preserves_whole_input_state(conn):
    conn.execute("CREATE TABLE sent AS SELECT id FROM cases WHERE id='B'")
    conn.execute("CREATE TABLE fanout AS SELECT a.id FROM sent a CROSS JOIN cases b")
    # A/B and C/B conflict. The B/B pair still has only contributor B.
    assert conn.fetchall("INSERT INTO updates SELECT id, TRUE FROM fanout") == [(1,)]
    conn.execute("CREATE TABLE diamond AS SELECT a.id FROM sent a JOIN sent b ON a.id=b.id")
    assert conn.fetchall("INSERT INTO updates SELECT id, TRUE FROM diamond") == [(1,)]


def test_literal_cannot_forge_required_source_and_metadata_is_hidden(conn):
    assert conn.fetchall("INSERT INTO updates SELECT 'B', TRUE") == [(0,)]
    conn.execute("CREATE TABLE sent AS SELECT id FROM cases WHERE id='B'")
    result = conn.execute("SELECT s.* FROM sent s")
    assert [c[0] for c in result.description] == ["id"]
    assert result.fetchone() == ("B",)
    assert len(result.intermediates) == 1
    assert conn.last_intermediate_result() is result
    with pytest.raises(IntermediateError, match="reserved"):
        conn.execute("SELECT _dfc_intermediate_state FROM sent")


def test_unsupported_queries_fail_without_effects(conn):
    for sql in [
        "UPDATE cases SET amount=0",
        "DELETE FROM cases",
        "CREATE TABLE nope AS SELECT DISTINCT id FROM cases",
        "INSERT INTO updates SELECT id, TRUE FROM cases WHERE EXISTS (SELECT 1)",
    ]:
        with pytest.raises(IntermediateError):
            conn.execute(sql)
    assert conn.fetchall("SELECT COUNT(*) AS n FROM cases") == [(3,)]
    assert conn.fetchall("SELECT * FROM updates") == []


def test_trusted_tool_bridge_uses_normal_query_states(conn):
    conn.execute("CREATE TABLE observations(message_id VARCHAR, success BOOLEAN)")
    inputs = conn.execute("SELECT id FROM cases WHERE id='B'")
    assert inputs.fetchall() == [("B",)]
    # The SQL tool model selects these input rows. Both outputs inherit the
    # entire input; neither claims a subset of its original contributors.
    state = conn.merge_intermediate_inputs(inputs.intermediates[0])
    conn.store_intermediate_outputs("observations", [("m1", True), ("m2", True)], [state, state])
    assert conn.fetchall("INSERT INTO updates SELECT 'A', BOOL_AND(success) FROM observations") == [
        (0,)
    ]
    assert conn.fetchall("INSERT INTO updates SELECT 'B', BOOL_AND(success) FROM observations") == [
        (1,)
    ]


def test_insertion_coerces_sink_before_policy_check():
    with dfc(duckdb.connect()) as c:
        c.execute("CREATE TABLE source(id INT, value DOUBLE)")
        c.execute("INSERT INTO source VALUES (1, 1.5)")
        c.execute("CREATE TABLE target(value INTEGER)")
        c.register_policy(
            Policy.from_pgn(
                "SOURCE REQUIRED source SINK target CONSTRAINT source.value = target.value ON FAIL REMOVE"
            )
        )
        c.enable_intermediate_propagation()
        c.execute("CREATE TABLE saved AS SELECT value FROM source")
        # 1.5 is coerced to 2 at the sink; checking the pre-cast 1.5 would allow it.
        assert c.fetchall("INSERT INTO target SELECT value FROM saved") == [(0,)]
        assert c.fetchall("SELECT * FROM target") == []


def test_kill_rejects_the_entire_statement_before_persistence():
    with dfc(duckdb.connect()) as c:
        c.execute("CREATE TABLE source(id INT)")
        c.execute("INSERT INTO source VALUES (1),(2)")
        c.execute("CREATE TABLE target(id INT)")
        c.register_policy(
            Policy.from_pgn(
                "SOURCE REQUIRED source SINK target CONSTRAINT source.id = target.id ON FAIL KILL"
            )
        )
        c.enable_intermediate_propagation()
        c.execute("CREATE TABLE saved AS SELECT id FROM source")
        with pytest.raises(duckdb.Error, match="KILL|kill"):
            c.execute("INSERT INTO target SELECT 1 FROM saved")
        assert c.fetchall("SELECT * FROM target") == []


def test_database_error_rolls_back_rows_and_state(conn):
    conn.execute("CREATE TABLE unique_target(id VARCHAR PRIMARY KEY)")
    with pytest.raises(duckdb.ConstraintException):
        conn.execute("INSERT INTO unique_target SELECT 'same' FROM cases")
    assert conn.last_intermediate_result() is None
    assert conn.fetchall("SELECT * FROM unique_target") == []
    assert conn.fetchall("INSERT INTO unique_target SELECT id FROM cases") == [(3,)]
    assert conn.fetchall("SELECT * FROM unique_target ORDER BY id") == [("A",), ("B",), ("C",)]


def test_automatic_materialization_rejects_lossy_public_timestamp(conn):
    with pytest.raises(IntermediateError, match="TIMESTAMP_NS"):
        conn.execute(
            "CREATE TABLE precise AS SELECT '2026-01-01 00:00:00.000000001'::TIMESTAMP_NS AS ts"
        )
    with pytest.raises(IntermediateError, match="missing catalog"):
        conn.execute("SELECT * FROM precise")


def test_nested_exists_is_not_misinterpreted_as_direct_join(conn):
    with pytest.raises(IntermediateError, match="expression subqueries"):
        conn.execute("SELECT id FROM cases WHERE EXISTS (SELECT 1 FROM cases other)")


def test_nested_materialized_aggregates_and_duplicate_paths_count_once():
    with dfc(duckdb.connect()) as c:
        c.execute("CREATE TABLE source(id INT, batch INT, amount INT)")
        c.execute("INSERT INTO source VALUES (1,1,10),(2,1,20),(3,2,30)")
        c.execute("CREATE TABLE copies(id INT)")
        c.execute("INSERT INTO copies VALUES (1),(2)")
        c.execute("CREATE TABLE target(total INT, mean DOUBLE)")
        c.register_policy(
            Policy.from_pgn("""SOURCE REQUIRED source SINK target
            CONSTRAINT SUM(source.amount) = target.total AND AVG(source.amount) = target.mean
            ON FAIL REMOVE""")
        )
        c.enable_intermediate_propagation()
        c.execute(
            "CREATE TABLE batches AS SELECT batch, SUM(amount) AS total FROM source GROUP BY batch"
        )
        c.execute("CREATE TABLE combined AS SELECT SUM(total) AS total FROM batches")
        assert c.fetchall("INSERT INTO target SELECT total, 20.0 FROM combined") == [(1,)]
        # Fan-out repeats the same whole state, and must not double its SUM.
        c.execute("CREATE TABLE repeated AS SELECT total FROM combined CROSS JOIN copies")
        assert c.fetchall("INSERT INTO target SELECT SUM(total), 20.0 FROM repeated") == [(0,)]
        assert c.fetchall("INSERT INTO target SELECT MAX(total), 20.0 FROM repeated") == [(1,)]


def test_connection_catalog_hides_persisted_state(conn):
    conn.execute("CREATE TABLE saved AS SELECT id FROM cases WHERE id='B'")
    snapshot = conn._introspect_catalog()
    assert all(
        not column.startswith("_dfc_intermediate_")
        for table in snapshot["tables"].values()
        for column in table["columns"]
    )
    with pytest.raises(ValueError, match="owning connection"):
        conn.transform_query("SELECT * FROM saved")


def test_dynamic_and_whole_row_projection_cannot_expose_state(conn):
    conn.execute("CREATE TABLE saved AS SELECT id FROM cases WHERE id='B'")
    for query in [
        "SELECT to_json(saved) AS value FROM saved",
        "SELECT COLUMNS('.*') FROM saved",
        "SELECT COLUMNS('^_dfc') AS leaked FROM saved",
        "SELECT id FROM saved ORDER BY 2",
        "SELECT saved.* EXCLUDE(id) FROM saved",
    ]:
        with pytest.raises(IntermediateError):
            conn.execute(query)


@pytest.mark.parametrize("entrypoint", ["execute", "fetchall", "fetchone"])
@pytest.mark.parametrize(
    "attack",
    [
        "INSERT INTO cases SELECT 'A','public'",
        "INSERT INTO \"CASES\" (id,category) SELECT 'A','public'",
        "CREATE TEMP TABLE cases AS SELECT 'A' AS id,'public' AS category",
        'CREATE TEMP TABLE "CASES" (id VARCHAR,category VARCHAR)',
    ],
)
def test_ordinary_sql_cannot_forge_or_shadow_authoritative_sources(entrypoint, attack):
    with dfc(duckdb.connect()) as c:
        c.execute("CREATE TABLE cases(id VARCHAR,category VARCHAR)")
        c.execute("INSERT INTO cases VALUES ('A','secret')")
        c.execute("CREATE TABLE updates(id VARCHAR)")
        c.register_policy(
            Policy.from_pgn("""SOURCE REQUIRED cases SINK updates
            CONSTRAINT cases.category='public' AND cases.id=updates.id ON FAIL REMOVE""")
        )
        c.enable_intermediate_propagation()
        assert c.fetchall("INSERT INTO updates SELECT id FROM cases") == [(0,)]
        with pytest.raises(IntermediateError, match="declared SOURCE"):
            getattr(c, entrypoint)(attack)
        assert c.last_intermediate_result() is None
        assert c.fetchall("SELECT * FROM cases") == [("A", "secret")]
        assert c.fetchall("INSERT INTO updates SELECT id FROM cases WHERE category='public'") == [
            (0,)
        ]
        assert c.fetchall("SELECT * FROM updates") == []


@pytest.mark.parametrize(
    "attack",
    [
        "INSERT INTO main.cases SELECT 'A','public','High',10",
        "CREATE TEMP TABLE main.cases AS SELECT 'A' AS id",
        "COPY cases FROM 'fabricated.csv'",
        "ALTER TABLE cases RENAME TO discarded",
        "TRUNCATE cases",
        "CREATE VIEW cases AS SELECT 'A' AS id",
        "CREATE OR REPLACE TABLE cases AS SELECT 'A' AS id",
    ],
)
def test_other_source_write_entrypoints_fail_closed(conn, attack):
    with pytest.raises(IntermediateError):
        conn.execute(attack)
    assert conn.fetchall("SELECT COUNT(*) AS n FROM cases") == [(3,)]


@pytest.mark.parametrize("replacement", ["table", "view", "recreated"])
def test_physical_source_binding_detects_same_name_replacement(conn, replacement):
    # Raw catalog mutation is outside the model SQL API. Nevertheless, the next
    # execution must reject a changed binding rather than trusting its spelling.
    raw = conn.raw_connection
    if replacement == "recreated":
        raw.execute("DROP TABLE cases")
        raw.execute(
            "CREATE TABLE cases AS SELECT 'B' AS id, 'Benefits' AS category, 'High' AS priority, 1 AS amount"
        )
    else:
        raw.execute(
            f"CREATE TEMP {replacement.upper()} cases AS SELECT 'B' AS id, 'Benefits' AS category, 'High' AS priority, 1 AS amount"
        )
    with pytest.raises(IntermediateError, match="Physical SOURCE binding"):
        conn.execute("INSERT INTO updates SELECT id, TRUE FROM cases")
    assert raw.execute("SELECT * FROM updates").fetchall() == []


def test_preexisting_source_shadow_is_rejected_when_enabling():
    with dfc(duckdb.connect()) as c:
        c.execute("CREATE TABLE cases(id INT)")
        c.execute("CREATE TABLE updates(id INT)")
        c.register_policy(
            Policy.from_pgn(
                "SOURCE REQUIRED cases SINK updates CONSTRAINT cases.id=updates.id ON FAIL REMOVE"
            )
        )
        c.raw_connection.execute("CREATE TEMP TABLE cases(id INT)")
        with pytest.raises(IntermediateError, match="unambiguous base table"):
            c.enable_intermediate_propagation()


def test_trusted_source_observation_append_refreshes_only_its_own_binding(conn):
    # This is explicitly trusted harness authority, never a model SQL endpoint.
    # A real external observation may introduce a fresh source contributor.
    state = conn.merge_intermediate_inputs()
    conn.store_intermediate_outputs("cases", [("D", "Benefits", "High", 10)], [state])
    assert conn.fetchall("INSERT INTO updates SELECT id, TRUE FROM cases WHERE id='D'") == [(1,)]
    with pytest.raises(IntermediateError, match="declared SOURCE"):
        conn.execute("INSERT INTO cases SELECT 'E','Benefits','High',10")


def test_native_ctes_derived_tables_and_having(conn):
    conn.execute("""CREATE TABLE grouped AS
        WITH chosen AS (SELECT * FROM cases WHERE id='B'),
             counted(case_id, n) AS (SELECT id, COUNT(*) FROM chosen GROUP BY id HAVING COUNT(*)>0)
        SELECT x.* FROM (SELECT * FROM counted) x""")
    assert conn.fetchall("INSERT INTO updates SELECT case_id, n=1 FROM grouped") == [(1,)]
    assert conn.fetchall("INSERT INTO updates SELECT 'A', n=1 FROM grouped") == [(0,)]


def test_cte_source_name_shadow_does_not_mint_authority(conn):
    assert conn.fetchall("""INSERT INTO updates
        WITH cases AS (SELECT 'B' AS id)
        SELECT id, TRUE FROM cases""") == [(0,)]


def test_cte_parameters_and_nested_scope(conn):
    result = conn.execute(
        """WITH chosen AS (SELECT id FROM cases WHERE id=?)
        SELECT d.id FROM (WITH chosen AS (SELECT id FROM chosen WHERE id=?)
        SELECT id FROM chosen) d""",
        params=["B", "B"],
    )
    assert result.fetchall() == [("B",)]
    conn.store_intermediate_outputs("updates", [("B", True)], result.intermediates)
    assert conn.fetchall("SELECT * FROM updates") == [("B", True)]


def test_window_frame_states_propagate_through_cte_and_materialization(conn):
    conn.execute("""CREATE TABLE framed AS
        WITH w AS (SELECT id, SUM(amount) OVER
          (ORDER BY id ROWS BETWEEN CURRENT ROW AND 1 FOLLOWING) AS total FROM cases)
        SELECT * FROM w""")
    # B's frame includes C, so a fixed B output cannot claim unique B provenance.
    assert conn.fetchall("INSERT INTO updates SELECT id, TRUE FROM framed WHERE id='B'") == [(0,)]
    conn.execute("""CREATE TABLE singleton AS SELECT id,
        SUM(amount) OVER (PARTITION BY id) AS total,
        MAX(amount) OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND CURRENT ROW) AS largest FROM cases""")
    assert conn.fetchall(
        "INSERT INTO updates SELECT id, total=largest FROM singleton WHERE id='B'"
    ) == [(1,)]


def test_grouped_cte_then_window_and_reused_parameters(conn):
    result = conn.execute(
        """WITH g AS (SELECT id, SUM(amount) AS amount FROM cases GROUP BY id)
        SELECT id, SUM(amount) OVER (PARTITION BY id ORDER BY amount
        ROWS BETWEEN ? PRECEDING AND CURRENT ROW) AS total FROM g WHERE id=?""",
        params=[1, "B"],
    )
    assert result.fetchall() == [("B", 20)]
    conn.store_intermediate_outputs("updates", [("B", True)], result.intermediates)


@pytest.mark.parametrize(
    "expression",
    [
        "ROW_NUMBER() OVER (ORDER BY id)",
        "ROW_NUMBER() OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND CURRENT ROW)",
        "LAG(amount) OVER (ORDER BY id)",
        "RANK() OVER (ORDER BY id)",
        "SUM(amount) FILTER (WHERE amount>10) OVER ()",
    ],
)
def test_windows_without_supported_contributor_contract_fail_closed(conn, expression):
    with pytest.raises(IntermediateError, match="window"):
        conn.execute(f"CREATE TABLE rejected AS SELECT id, {expression} AS x FROM cases")
    assert conn.raw_connection.execute(
        "SELECT COUNT(*) FROM duckdb_tables() WHERE table_name='rejected'"
    ).fetchone() == (0,)


def test_retry_reports_stable_actual_policy_ids_and_rolls_back():
    from data_flow_control import Resolution, RetryViolation

    with dfc(duckdb.connect()) as c:
        c.execute("CREATE TABLE source(id INT, amount INT)")
        c.execute("INSERT INTO source VALUES (1,10),(2,20)")
        c.execute("CREATE TABLE target(id INT)")
        removed = Policy(
            sources=["source"],
            sink="target",
            constraint="SUM(source.amount)<0",
            on_fail=Resolution.RETRY,
            retry_message="deleted",
        )
        c.register_policy(removed)
        c.register_policy(
            Policy(
                sources=["source"],
                sink="target",
                constraint="SUM(source.amount)<15",
                on_fail=Resolution.RETRY,
                retry_message="same",
            )
        )
        c.register_policy(
            Policy(
                sources=["source"],
                sink="target",
                constraint="SUM(source.amount)<18",
                on_fail=Resolution.RETRY,
                retry_message="same",
            )
        )
        c.register_policy(
            Policy(
                sources=["source"],
                sink="target",
                constraint="SUM(source.amount)<100",
                on_fail=Resolution.RETRY,
                retry_message="passing",
            )
        )
        assert c.delete_policy(sources=["source"], sink="target", constraint=removed.constraint)
        c.enable_intermediate_propagation()
        c.execute("CREATE TABLE intermediate AS WITH s AS (SELECT * FROM source) SELECT id FROM s")
        with pytest.raises(RetryViolation) as caught:
            c.execute("INSERT INTO target SELECT id FROM intermediate")
        assert [(f.policy_index, f.message) for f in caught.value.failures] == [
            (1, "same"),
            (2, "same"),
        ]
        assert c.raw_connection.execute("SELECT * FROM target").fetchall() == []
        assert c.last_intermediate_result() is None
        assert c.fetchall("INSERT INTO target SELECT id FROM intermediate WHERE id=1") == [(1,)]


def test_retry_remove_and_kill_precedence():
    from data_flow_control import Resolution, RetryViolation

    for kill in (False, True):
        with dfc(duckdb.connect()) as c:
            c.execute("CREATE TABLE source(id INT)")
            c.execute("INSERT INTO source VALUES (1),(2)")
            c.execute("CREATE TABLE target(id INT)")
            c.register_policy(
                Policy(
                    sources=["source"],
                    sink="target",
                    constraint="source.id<0",
                    on_fail=Resolution.RETRY,
                    retry_message="retry",
                )
            )
            c.register_policy(
                Policy(
                    sources=["source"],
                    sink="target",
                    constraint="source.id<0",
                    on_fail=Resolution.REMOVE,
                )
            )
            if kill:
                c.register_policy(
                    Policy(
                        sources=["source"],
                        sink="target",
                        constraint="source.id<0",
                        on_fail=Resolution.KILL,
                    )
                )
            c.enable_intermediate_propagation()
            if kill:
                with pytest.raises(duckdb.Error) as caught:
                    c.execute("INSERT INTO target SELECT id FROM source")
                assert not isinstance(caught.value, RetryViolation)
            else:
                assert c.fetchall("INSERT INTO target SELECT id FROM source") == [(0,)]
            assert c.raw_connection.execute("SELECT * FROM target").fetchall() == []


@pytest.mark.parametrize(
    "tail",
    [
        "LIMIT (SELECT amount FROM cases LIMIT 1)",
        "OFFSET (SELECT amount FROM cases LIMIT 1)",
    ],
)
def test_limit_offset_subqueries_cannot_read_untracked_sources(conn, tail):
    with pytest.raises(IntermediateError, match="expression subqueries"):
        conn.execute(f"INSERT INTO updates SELECT 'B', TRUE {tail}")
    assert conn.raw_connection.execute("SELECT * FROM updates").fetchall() == []


def test_having_only_aggregate_captures_group_contributors(conn):
    conn.execute("CREATE TABLE sent AS SELECT 'B' AS id FROM cases WHERE id='B' HAVING COUNT(*)>0")
    assert conn.fetchall("INSERT INTO updates SELECT id, TRUE FROM sent") == [(1,)]
    conn.execute("CREATE TABLE conflict AS SELECT 'B' AS id FROM cases HAVING COUNT(*)>0")
    assert conn.fetchall("INSERT INTO updates SELECT id, TRUE FROM conflict") == [(0,)]


def test_delete_and_reregister_identical_policy_invalidates_session(conn):
    policy = conn.policies()[0]
    assert conn.delete_policy(
        sources=policy.sources, sink=policy.sink, constraint=policy.constraint
    )
    conn.register_policy(policy)
    with pytest.raises(IntermediateError, match="Policy changes"):
        conn.execute("SELECT id FROM cases")


@pytest.mark.parametrize(
    "sql",
    [
        "SELECT list_extract(ARRAY(SELECT amount FROM cases),1) AS amount",
        "SELECT 1 AS x WHERE list_count(ARRAY(SELECT amount FROM cases))>0",
        "SELECT 1 AS x FROM cases a JOIN cases b ON list_count(ARRAY(SELECT id FROM cases))>0",
    ],
)
def test_array_subquery_cannot_bypass_source_capture(conn, sql):
    with pytest.raises(IntermediateError, match="expression subqueries"):
        conn.execute(sql)


@pytest.mark.parametrize(
    "sql",
    [
        "SELECT 1 AS x FROM sent s WHERE CAST(s AS VARCHAR) LIKE '%1%'",
        "SELECT 1 AS x FROM sent s JOIN sent t ON struct_extract(s, '_dfc_intermediate_state')=struct_extract(t, '_dfc_intermediate_state')",
        "SELECT 1 AS x FROM sent s ORDER BY CAST(s AS VARCHAR)",
    ],
)
def test_whole_row_metadata_is_hidden_in_all_expression_scopes(conn, sql):
    conn.execute("CREATE TABLE sent AS SELECT id FROM cases WHERE id='B'")
    with pytest.raises(IntermediateError, match="whole-row"):
        conn.execute(sql)


def test_secret_predecessor_cannot_leak_through_public_row_ordinal():
    with dfc(duckdb.connect()) as c:
        c.execute("CREATE TABLE source(id INT, public BOOLEAN)")
        c.execute("INSERT INTO source VALUES (1,FALSE),(2,TRUE)")
        c.execute("CREATE TABLE target(id INT, ordinal BIGINT)")
        c.register_policy(
            Policy.from_pgn("""SOURCE REQUIRED source SINK target
            CONSTRAINT BOOL_AND(source.public) ON FAIL REMOVE""")
        )
        # Public payload unchanged; the secret predecessor changes its ordinal.
        assert c.raw_connection.execute(
            "SELECT id, ROW_NUMBER() OVER(ORDER BY id) AS ordinal FROM source"
        ).fetchall() == [(1, 1), (2, 2)]
        assert c.raw_connection.execute(
            "SELECT id, ROW_NUMBER() OVER(ORDER BY id) AS ordinal FROM source WHERE public"
        ).fetchall() == [(2, 1)]
        c.enable_intermediate_propagation()
        with pytest.raises(IntermediateError, match="ranking"):
            c.execute("""INSERT INTO target WITH ranked AS
                (SELECT id, ROW_NUMBER() OVER(ORDER BY id) AS ordinal FROM source)
                SELECT * FROM ranked WHERE id=2""")
        with pytest.raises(IntermediateError, match="ranking"):
            c.execute(
                "CREATE TABLE ranked AS SELECT id, ROW_NUMBER() OVER(ORDER BY id) AS ordinal FROM source"
            )
        assert c.raw_connection.execute("SELECT * FROM target").fetchall() == []
        assert c.raw_connection.execute(
            "SELECT COUNT(*) FROM duckdb_tables() WHERE table_name='ranked'"
        ).fetchone() == (0,)
