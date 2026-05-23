#[path = "../common/mod.rs"]
mod common;

#[path = "../common/duckdb.rs"]
mod duckdb;

#[path = "aggregate_finalize.rs"]
mod aggregate_finalize;
#[path = "insert.rs"]
mod insert;
#[path = "invalidate.rs"]
mod invalidate;
#[path = "kill.rs"]
mod kill;
#[path = "remove.rs"]
mod remove;
#[path = "update.rs"]
mod update;
