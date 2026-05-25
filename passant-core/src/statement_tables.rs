use std::collections::HashSet;

use sqlparser::ast::{Query, Statement};

use crate::identifiers::TableKey;
use crate::source_sets::{
    set_expr_source_tables, table_factor_source_tables, table_with_joins_source_tables,
};

/// Collect normalized base-table keys referenced by a parsed statement.
pub fn statement_table_keys(statement: &Statement) -> HashSet<TableKey> {
    match statement {
        Statement::Query(query) => query_table_keys(query),
        Statement::Insert(insert) => {
            let mut keys = HashSet::new();
            keys.insert(TableKey::new(&insert.table_name.to_string()));
            if let Some(source) = &insert.source {
                keys.extend(query_table_keys(source));
            }
            keys
        }
        Statement::Update { table, from, .. } => {
            let mut keys = table_with_joins_source_tables(table);
            if let Some(from) = from {
                keys.extend(table_with_joins_source_tables(from));
            }
            keys
        }
        Statement::Merge { table, source, .. } => {
            let mut keys = table_factor_source_tables(table);
            keys.extend(table_factor_source_tables(source));
            keys
        }
        _ => HashSet::new(),
    }
}

/// Normalized sink table key for write statements, if present.
pub fn statement_sink_key(statement: &Statement) -> Option<TableKey> {
    match statement {
        Statement::Insert(insert) => Some(TableKey::new(&insert.table_name.to_string())),
        Statement::Update { table, .. } => table_with_joins_source_tables(table).into_iter().next(),
        Statement::Merge { table, .. } => table_factor_source_tables(table).into_iter().next(),
        _ => None,
    }
}

fn query_table_keys(query: &Query) -> HashSet<TableKey> {
    let mut keys = set_expr_source_tables(query.body.as_ref());
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            keys.extend(set_expr_source_tables(cte.query.body.as_ref()));
        }
    }
    keys
}
