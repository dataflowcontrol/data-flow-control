use crate::common::pgn_policy;
use crate::duckdb::TestDb;

fn setup_foo_table(db: &mut TestDb) {
    db.exec("CREATE TABLE foo (id INTEGER, name VARCHAR, amount INTEGER)");
    db.exec("INSERT INTO foo VALUES (1, 'Alice', 5), (2, 'Bob', 15), (3, 'Charlie', 25)");
}

#[test]
fn remove_drops_rows_with_lt_constraint() {
    let mut db = TestDb::new();
    setup_foo_table(&mut db);
    db.register_policy(pgn_policy(&["foo"], "min(foo.id) < 3"));

    assert_eq!(
        db.rewrite_and_fetch_i64("SELECT id FROM foo ORDER BY id"),
        vec![1, 2]
    );
}

#[test]
fn remove_drops_rows_with_equality_constraint() {
    let mut db = TestDb::new();
    setup_foo_table(&mut db);
    db.register_policy(pgn_policy(&["foo"], "max(foo.id) = 2"));

    assert_eq!(
        db.rewrite_and_fetch_i64("SELECT id FROM foo ORDER BY id"),
        vec![2]
    );
}

#[test]
fn remove_drops_rows_with_ne_constraint() {
    let mut db = TestDb::new();
    setup_foo_table(&mut db);
    db.register_policy(pgn_policy(&["foo"], "max(foo.id) != 2"));

    assert_eq!(
        db.rewrite_and_fetch_i64("SELECT id FROM foo ORDER BY id"),
        vec![1, 3]
    );
}

#[test]
fn remove_drops_rows_with_or_constraint() {
    let mut db = TestDb::new();
    setup_foo_table(&mut db);
    db.register_policy(pgn_policy(&["foo"], "max(foo.id) = 1 OR max(foo.id) = 3"));

    assert_eq!(
        db.rewrite_and_fetch_i64("SELECT id FROM foo ORDER BY id"),
        vec![1, 3]
    );
}

#[test]
fn remove_drops_rows_with_and_constraint() {
    let mut db = TestDb::new();
    setup_foo_table(&mut db);
    db.register_policy(pgn_policy(&["foo"], "max(foo.id) > 1 AND min(foo.id) < 3"));

    assert_eq!(
        db.rewrite_and_fetch_i64("SELECT id FROM foo ORDER BY id"),
        vec![2]
    );
}

#[test]
fn remove_drops_all_rows_when_all_fail() {
    let mut db = TestDb::new();
    setup_foo_table(&mut db);
    db.register_policy(pgn_policy(&["foo"], "max(foo.id) > 10"));

    assert_eq!(
        db.rewrite_and_fetch_i64("SELECT id FROM foo ORDER BY id"),
        Vec::<i64>::new()
    );
}

#[test]
fn remove_keeps_all_rows_when_all_pass() {
    let mut db = TestDb::new();
    setup_foo_table(&mut db);
    db.register_policy(pgn_policy(&["foo"], "max(foo.id) >= 1"));

    assert_eq!(
        db.rewrite_and_fetch_i64("SELECT id FROM foo ORDER BY id"),
        vec![1, 2, 3]
    );
}

#[test]
fn remove_drops_rows_with_count_if() {
    let mut db = TestDb::new();
    setup_foo_table(&mut db);
    db.register_policy(pgn_policy(&["foo"], "COUNT_IF(foo.id > 2) > 0"));

    assert_eq!(
        db.rewrite_and_fetch_i64("SELECT id FROM foo ORDER BY id"),
        vec![3]
    );
}

#[test]
fn remove_drops_rows_with_string_comparison() {
    let mut db = TestDb::new();
    setup_foo_table(&mut db);
    db.register_policy(pgn_policy(&["foo"], "max(foo.name) != 'Alice'"));

    assert_eq!(
        db.rewrite_and_fetch_strings("SELECT name FROM foo ORDER BY name"),
        vec!["Bob".to_string(), "Charlie".to_string()]
    );
}

#[test]
fn remove_drops_violating_aggregation_groups() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (category VARCHAR, amount INTEGER)");
    db.exec("INSERT INTO foo VALUES ('a', 1), ('a', 5), ('b', 10)");
    db.register_policy(pgn_policy(&["foo"], "max(foo.amount) > 6"));

    let rewritten = db
        .rewrite(
            "SELECT category, SUM(amount) AS total FROM foo GROUP BY category ORDER BY category",
        )
        .expect("aggregation should rewrite");
    assert_eq!(
        db.fetchall_i64(&format!(
            "SELECT total FROM ({rewritten}) ORDER BY category"
        )),
        vec![10]
    );
}

#[test]
fn remove_drops_scalar_aggregation_when_constraint_fails() {
    let mut db = TestDb::new();
    setup_foo_table(&mut db);
    db.register_policy(pgn_policy(&["foo"], "max(foo.id) > 10"));

    let rewritten = db
        .rewrite("SELECT MAX(id) FROM foo")
        .expect("aggregation should rewrite");
    assert_eq!(db.fetchall_i64(&rewritten), Vec::<i64>::new());
}
