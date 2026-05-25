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
        self.rewriter.register_policy(policy);
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

    pub fn finalize_aggregate_policies(&self, sink_table: &str) {
        for query in self.rewriter.finalize_aggregate_queries(sink_table) {
            if let Some(invalidate_sql) = query.invalidate_sql {
                self.exec(&invalidate_sql);
            }
        }
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
