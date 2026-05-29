use assert_cmd::Command;
use predicates::prelude::*;

fn passant_cmd() -> Command {
    Command::cargo_bin("passant").expect("passant binary should exist")
}

#[test]
fn rewrite_subcommand_prints_rewritten_sql() {
    passant_cmd()
        .args([
            "rewrite",
            "SELECT id FROM foo",
            "--policy",
            "SOURCE foo CONSTRAINT max(foo.id) > 1 ON FAIL REMOVE",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "SELECT id FROM foo WHERE foo.id > 1",
        ));
}

#[test]
fn explain_subcommand_prints_json_with_strategy() {
    passant_cmd()
        .args([
            "explain",
            "SELECT id FROM foo",
            "--policy",
            "SOURCE foo CONSTRAINT max(foo.id) > 1 ON FAIL REMOVE",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"strategy\""));
}

#[test]
fn plan_subcommand_prints_chosen_strategy() {
    passant_cmd()
        .args([
            "plan",
            "SELECT id FROM foo",
            "--policy",
            "SOURCE foo CONSTRAINT max(foo.id) > 1 ON FAIL REMOVE",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("FullPush"));
}

#[test]
fn parse_policy_text_subcommand_prints_policy_json() {
    passant_cmd()
        .args([
            "parse-policy",
            "--text",
            "SOURCE foo SINK reports CONSTRAINT max(foo.id) > 1 ON FAIL REMOVE",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"Pgn\""));
}

#[test]
fn parse_policy_flag_subcommand_builds_policy_from_parts() {
    passant_cmd()
        .args([
            "parse-policy",
            "--source",
            "foo",
            "--sink",
            "reports",
            "--constraint",
            "max(foo.id) > 1",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("max(foo.id) > 1"));
}

#[test]
fn rewrite_rejects_invalid_sql() {
    passant_cmd()
        .args([
            "rewrite",
            "SELECT FROM",
            "--policy",
            "SOURCE foo CONSTRAINT max(foo.id) > 1 ON FAIL REMOVE",
        ])
        .assert()
        .failure();
}

#[test]
fn rewrite_applies_multiple_policies() {
    passant_cmd()
        .args([
            "rewrite",
            "SELECT id FROM foo",
            "--policy",
            "SOURCE foo CONSTRAINT max(foo.id) > 1 ON FAIL REMOVE",
            "--policy",
            "SOURCE foo CONSTRAINT max(foo.id) < 100 ON FAIL REMOVE",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("foo.id > 1"));
}
