#[path = "../common/mod.rs"]
mod common;

#[path = "../common/duckdb.rs"]
mod duckdb;

mod insert;
#[path = "kill.rs"]
mod kill;
#[path = "oracle.rs"]
mod oracle;
#[path = "remove.rs"]
mod remove;
#[path = "update.rs"]
mod update;
