use duckdb::Connection;
use duckdb::core::{DataChunkHandle, LogicalTypeHandle, LogicalTypeId};
use duckdb::vscalar::{ScalarFunctionSignature, VScalar};
use duckdb::vtab::arrow::WritableVector;
use passant_core::{PassantRewriter, PolicyIr, RewriteError};

#[allow(dead_code)]
pub struct TestDb {
    pub conn: Connection,
    pub rewriter: PassantRewriter,
}

struct KillScalar;

impl VScalar for KillScalar {
    type State = ();

    unsafe fn invoke(
        _: &Self::State,
        input: &mut DataChunkHandle,
        _: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let _ = input.len();
        Err("KILLing due to dfc policy violation".into())
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![],
            LogicalTypeHandle::from(LogicalTypeId::Boolean),
        )]
    }

    fn volatile() -> bool {
        true
    }
}

fn register_passant_udfs(conn: &Connection) -> duckdb::Result<()> {
    conn.register_scalar_function::<KillScalar>("kill")?;
    conn.register_scalar_function::<KillScalar>("passant_kill")?;
    Ok(())
}

#[allow(dead_code)]
impl TestDb {
    pub fn new() -> Self {
        let conn = Connection::open_in_memory().expect("duckdb should open in memory");
        register_passant_udfs(&conn).expect("passant udfs should register");
        Self {
            conn,
            rewriter: PassantRewriter::new(),
        }
    }

    pub fn exec(&self, sql: &str) {
        self.conn
            .execute_batch(sql)
            .unwrap_or_else(|err| panic!("exec failed for {sql:?}: {err}"));
    }

    pub fn fetchall_i64(&self, sql: &str) -> Vec<i64> {
        let mut stmt = self
            .conn
            .prepare(sql)
            .unwrap_or_else(|err| panic!("prepare failed for {sql:?}: {err}"));
        let rows = stmt
            .query_map([], |row| row.get(0))
            .unwrap_or_else(|err| panic!("query failed for {sql:?}: {err}"));
        rows.map(|row| row.expect("row should decode")).collect()
    }

    pub fn fetchall_bool(&self, sql: &str) -> Vec<bool> {
        let mut stmt = self
            .conn
            .prepare(sql)
            .unwrap_or_else(|err| panic!("prepare failed for {sql:?}: {err}"));
        let rows = stmt
            .query_map([], |row| row.get(0))
            .unwrap_or_else(|err| panic!("query failed for {sql:?}: {err}"));
        rows.map(|row| row.expect("row should decode")).collect()
    }

    pub fn fetchall_strings(&self, sql: &str) -> Vec<String> {
        let mut stmt = self
            .conn
            .prepare(sql)
            .unwrap_or_else(|err| panic!("prepare failed for {sql:?}: {err}"));
        let rows = stmt
            .query_map([], |row| row.get(0))
            .unwrap_or_else(|err| panic!("query failed for {sql:?}: {err}"));
        rows.map(|row| row.expect("row should decode")).collect()
    }

    pub fn register_policy(&mut self, policy: PolicyIr) {
        // These schemas come from the fixture's real database, never from a
        // policy's requested identifiers. Registration therefore catches typos.
        let mut statement = self.conn.prepare(
            "SELECT table_schema, table_name, column_name, data_type FROM information_schema.columns WHERE table_schema NOT IN ('information_schema','pg_catalog') ORDER BY table_schema, table_name, ordinal_position",
        ).expect("fixture catalog introspection should prepare");
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .expect("fixture catalog introspection should execute");
        let mut tables = std::collections::BTreeMap::<String, Vec<(String, String)>>::new();
        for row in rows {
            let (schema, table, column, data_type) = row.expect("catalog row should decode");
            tables
                .entry(format!("{schema}.{table}"))
                .or_default()
                .push((column.clone(), data_type.clone()));
            if schema == "main" || schema == "temp" {
                tables.entry(table).or_default().push((column, data_type));
            }
        }
        for (table, columns) in tables {
            self.rewriter.catalog_mut().register_table(
                table.clone(),
                columns.iter().map(|(name, _)| name.clone()).collect(),
            );
            for (column, data_type) in columns {
                self.rewriter
                    .catalog_mut()
                    .register_column_type(table.clone(), column, data_type);
            }
        }
        self.rewriter
            .register_policy(policy)
            .expect("policy should match explicit fixture database schema");
    }

    pub fn rewrite(&self, sql: &str) -> Result<String, RewriteError> {
        self.rewriter.rewrite(sql)
    }

    pub fn rewrite_and_fetch_i64(&self, sql: &str) -> Vec<i64> {
        let rewritten = self
            .rewrite(sql)
            .unwrap_or_else(|err| panic!("rewrite failed for {sql:?}: {err}"));
        self.fetchall_i64(&rewritten)
    }

    pub fn rewrite_and_fetch_bool(&self, sql: &str) -> Vec<bool> {
        let rewritten = self
            .rewrite(sql)
            .unwrap_or_else(|err| panic!("rewrite failed for {sql:?}: {err}"));
        self.fetchall_bool(&rewritten)
    }

    pub fn rewrite_and_fetch_strings(&self, sql: &str) -> Vec<String> {
        let rewritten = self
            .rewrite(sql)
            .unwrap_or_else(|err| panic!("rewrite failed for {sql:?}: {err}"));
        self.fetchall_strings(&rewritten)
    }

    pub fn rewrite_exec(&self, sql: &str) {
        let rewritten = self
            .rewrite(sql)
            .unwrap_or_else(|err| panic!("rewrite failed for {sql:?}: {err}"));
        self.exec(&rewritten);
    }

    pub fn run_rewritten_expect_error(&self, sql: &str) -> String {
        let rewritten = self
            .rewrite(sql)
            .unwrap_or_else(|err| panic!("rewrite failed for {sql:?}: {err}"));
        match self.conn.prepare(&rewritten).and_then(|mut stmt| {
            let mut rows = stmt.query([])?;
            while rows.next()?.is_some() {}
            Ok(())
        }) {
            Ok(()) => panic!("expected error executing {rewritten:?}"),
            Err(err) => err.to_string(),
        }
    }
}
