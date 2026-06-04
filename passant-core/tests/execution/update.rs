use crate::duckdb::TestDb;
use passant_core::{PolicyIr, parse_policy_text};

fn state_transition_policy() -> PolicyIr {
    parse_policy_text(
        "SOURCE t AS t1 SINK t AS t2 CONSTRAINT count(distinct t1.id) = 1 AND max(t1.id) = t2.id AND case when max(t1.state) = 'A' then t2.state = 'B' when max(t1.state) = 'B' then t2.state in ('A', 'C') when max(t1.state) = 'C' then false end ON FAIL REMOVE",
    )
    .expect("state transition policy should parse")
}

#[test]
fn update_from_source_policy_filters_invalid_state_transition() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE t (id INTEGER, state VARCHAR)");
    db.exec("INSERT INTO t VALUES (1, 'A'), (2, 'A')");
    db.register_policy(state_transition_policy());

    db.rewrite_exec("UPDATE t AS t2 SET state = 'C' FROM t WHERE t.id = t2.id AND t.id = 1");
    assert_eq!(
        db.fetchall_strings("SELECT state FROM t WHERE id = 1"),
        vec!["A".to_string()]
    );
}
