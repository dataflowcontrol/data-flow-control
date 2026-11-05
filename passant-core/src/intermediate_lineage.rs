//! Catalog-backed export/import of row provenance for persisted intermediates.
//!
//! This is the data channel used by the execution adapter, not another policy
//! parser or resolution engine. Base scans export contributor identities;
//! persisted scans import opaque state tokens; CTEs/derived tables carry the
//! same channel through projections, joins, grouping and aggregate windows.
use std::ops::ControlFlow;

use sqlparser::ast::{Expr, Statement, Value, visit_expressions_mut};
use sqlparser::dialect::DuckDbDialect;
use sqlparser::parser::Parser;

use crate::diagnostics::RewriteError;
use crate::rewriter::PassantRewriter;

fn error(message: &str) -> RewriteError {
    RewriteError::unsupported_statement(format!("intermediate lineage: {message}"))
}

impl PassantRewriter {
    /// Stable registration IDs, including gaps left by deleted policies.
    pub fn registered_policy_indices(&self) -> Vec<usize> {
        self.store
            .policy_indices()
            .into_iter()
            .filter_map(|i| self.store.original_policy_index(i))
            .collect()
    }

    /// Rewrite a SELECT using the catalog's trusted intermediate channels.
    /// Generated channels are not revisited as user SQL or policy expressions.
    pub fn rewrite_intermediate_query(&self, sql: &str) -> Result<String, RewriteError> {
        let mut statements =
            Parser::parse_sql(&DuckDbDialect {}, sql).map_err(|e| error(&e.to_string()))?;
        if statements.len() != 1 {
            return Err(error("one SELECT statement is required"));
        }
        let Statement::Query(mut query) = statements.remove(0) else {
            return Err(error("one SELECT statement is required"));
        };
        // A generated window frame can repeat a parameter expression. Number
        // original positional parameters once so the clone keeps its binding.
        let mut index = 0;
        let _ = visit_expressions_mut(query.as_mut(), |expr| {
            if let Expr::Value(Value::Placeholder(name)) = expr
                && name == "?"
            {
                index += 1;
                *name = format!("${index}");
            }
            ControlFlow::<()>::Continue(())
        });
        self.rewrite_intermediate_ast(&mut query)?;
        Ok(query.to_string())
    }
}
