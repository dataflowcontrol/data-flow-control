#[path = "../common/mod.rs"]
mod common;

#[path = "../common/duckdb.rs"]
mod duckdb;

#[path = "column_propagation.rs"]
mod column_propagation;
#[path = "exists_join.rs"]
mod exists_join;
#[path = "having_where.rs"]
mod having_where;
#[path = "insert_shapes.rs"]
mod insert_shapes;
#[path = "multi_source.rs"]
mod multi_source;
#[path = "operators.rs"]
mod operators;
#[path = "query_shapes.rs"]
mod query_shapes;
