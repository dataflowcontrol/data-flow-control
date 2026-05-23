use crate::duckdb::TestDb;
use passant_core::{PolicyIr, Resolution, parse_policy_text};

fn state_transition_policy() -> PolicyIr {
    parse_policy_text(
        "SOURCE t AS t1 SINK t AS t2 CONSTRAINT count(distinct t.id) = 1 AND max(t.id) = t2.id AND case when max(t.state) = 'A' then t2.state = 'B' when max(t.state) = 'B' then t2.state in ('A', 'C') when max(t.state) = 'C' then false end ON FAIL REMOVE",
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

#[test]
fn update_invalidate_sets_valid_assignment() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE reports (id INTEGER, status VARCHAR, valid BOOLEAN)");
    db.exec("INSERT INTO reports VALUES (1, 'draft', TRUE)");
    db.register_policy(PolicyIr::CompatDfc {
        sources: Vec::new(),
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "reports.status = 'approved'".to_string(),
        on_fail: Resolution::Invalidate,
        description: None,
    });

    db.rewrite_exec("UPDATE reports SET status = 'draft', valid = valid");
    assert_eq!(db.fetchall_bool("SELECT valid FROM reports"), vec![false]);
}
