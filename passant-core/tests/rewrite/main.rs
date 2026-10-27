#[path = "../common/mod.rs"]
mod common;

#[path = "derived_policy.rs"]
mod derived_policy;
#[path = "in_semijoin.rs"]
mod in_semijoin;
#[path = "insert.rs"]
mod insert;
#[path = "joins.rs"]
mod joins;
#[path = "meal_deduction.rs"]
mod meal_deduction;
#[path = "output.rs"]
mod output;
#[path = "recursive.rs"]
mod recursive;
#[path = "scan.rs"]
mod scan;
#[path = "update.rs"]
mod update;

use crate::common::assert_same_rows;

fn assert_id_fixture_equivalent(actual: &str, reference: &str) {
    for data in [
        "",
        "INSERT INTO foo VALUES (NULL); INSERT INTO bar VALUES (NULL);",
        "INSERT INTO foo VALUES (1),(2),(2),(4),(NULL); INSERT INTO bar VALUES (1),(2),(2),(3),(NULL);",
    ] {
        assert_same_rows(
            actual,
            reference,
            &format!("CREATE TABLE foo(id INTEGER); CREATE TABLE bar(id INTEGER); {data}"),
        );
    }
}
