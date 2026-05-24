use std::collections::{HashMap, HashSet};

use sqlparser::ast::{Select, SetExpr, TableFactor, TableWithJoins};

use crate::identifiers::{Alias, AliasByBase, TableKey, TableName};

/// Visible tables and alias map for a SELECT.
#[derive(Debug, Default)]
pub(crate) struct TableScope {
    pub(crate) base_tables: HashSet<TableKey>,
    pub(crate) direct_base_tables: HashSet<TableKey>,
    /// Base table lookup key → query alias.
    pub(crate) alias_by_base: AliasByBase,
    pub(crate) aliases_by_base: HashMap<String, Vec<String>>,
}

impl TableScope {
    pub(crate) fn from_select(select: &Select) -> Self {
        let mut scope = Self::default();
        for table in &select.from {
            scope.add_table_with_joins(table);
        }
        scope
    }

    pub(crate) fn add_table_with_joins(&mut self, table: &TableWithJoins) {
        self.add_table_factor(&table.relation, true);
        for join in &table.joins {
            self.add_table_factor(&join.relation, true);
        }
    }

    pub(crate) fn alias_for(&self, base: &TableName) -> Option<&str> {
        self.alias_by_base.get(base)
    }

    fn add_table_factor(&mut self, factor: &TableFactor, is_direct: bool) {
        match factor {
            TableFactor::Table { name, alias, .. } => {
                let base = TableName::from_object_name(name);
                let key = TableKey::from_table(&base);
                self.base_tables.insert(key.clone());
                if is_direct {
                    self.direct_base_tables.insert(key.clone());
                }
                if let Some(alias) = alias {
                    let alias_name = Alias::new(alias.name.value.clone());
                    self.alias_by_base.insert(&base, &alias_name);
                    self.aliases_by_base
                        .entry(key.as_str().to_string())
                        .or_default()
                        .push(alias_name.as_str().to_string());
                }
            }
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                if let Some(alias) = alias {
                    self.base_tables
                        .insert(TableKey::new(alias.name.value.as_str()));
                }
                if let SetExpr::Select(select) = subquery.body.as_ref() {
                    for table in &select.from {
                        self.add_table_factor(&table.relation, false);
                        for join in &table.joins {
                            self.add_table_factor(&join.relation, false);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
