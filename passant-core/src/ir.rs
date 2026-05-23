use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExprRef {
    pub sql: String,
}

impl ExprRef {
    pub fn new(sql: impl Into<String>) -> Self {
        Self { sql: sql.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionItem {
    pub expr: ExprRef,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRef {
    pub relation_sql: String,
    pub condition_sql: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FromItem {
    pub relation_sql: String,
    pub alias: Option<String>,
    pub tables: Vec<TableRef>,
    pub joins: Vec<JoinRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub column: String,
    pub value: ExprRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassantSelect {
    pub projection: Vec<ProjectionItem>,
    pub from: Vec<FromItem>,
    pub where_clause: Option<ExprRef>,
    pub having: Option<ExprRef>,
    pub group_by: Vec<ExprRef>,
    pub order_by: Vec<ExprRef>,
    pub limit: Option<ExprRef>,
    pub ctes: Vec<String>,
    pub is_distinct: bool,
    pub raw_sql: String,
}

impl PassantSelect {
    pub fn visible_tables(&self) -> Vec<String> {
        let mut visible = Vec::new();
        for from_item in &self.from {
            for table in &from_item.tables {
                visible.push(table.alias.clone().unwrap_or_else(|| table.name.clone()));
            }
        }
        visible
    }

    pub fn is_aggregation(&self) -> bool {
        !self.group_by.is_empty()
            || self
                .projection
                .iter()
                .any(|item| contains_aggregate(&item.expr.sql))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryIr {
    Select(PassantSelect),
    InsertSelect {
        sink: TableRef,
        columns: Vec<String>,
        select: Box<PassantSelect>,
        raw_sql: String,
    },
    Update {
        sink: TableRef,
        assignments: Vec<Assignment>,
        from: Vec<FromItem>,
        where_clause: Option<ExprRef>,
        raw_sql: String,
    },
    Passthrough {
        statement_type: String,
        raw_sql: String,
    },
}

impl QueryIr {
    pub fn raw_sql(&self) -> &str {
        match self {
            QueryIr::Select(select) => &select.raw_sql,
            QueryIr::InsertSelect { raw_sql, .. } => raw_sql,
            QueryIr::Update { raw_sql, .. } => raw_sql,
            QueryIr::Passthrough { raw_sql, .. } => raw_sql,
        }
    }
}

fn contains_aggregate(sql: &str) -> bool {
    let upper = sql.to_ascii_uppercase();
    [
        "COUNT(",
        "SUM(",
        "AVG(",
        "MIN(",
        "MAX(",
        "ARRAY_AGG(",
        "BOOL_AND(",
        "BOOL_OR(",
    ]
    .iter()
    .any(|needle| upper.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_tables_prefers_alias_over_base_name() {
        let select = PassantSelect {
            projection: vec![],
            from: vec![FromItem {
                relation_sql: "foo AS f".to_string(),
                alias: Some("f".to_string()),
                tables: vec![TableRef {
                    name: "foo".to_string(),
                    alias: Some("f".to_string()),
                }],
                joins: vec![],
            }],
            where_clause: None,
            having: None,
            group_by: vec![],
            order_by: vec![],
            limit: None,
            ctes: vec![],
            is_distinct: false,
            raw_sql: String::new(),
        };
        assert_eq!(select.visible_tables(), vec!["f".to_string()]);
    }

    #[test]
    fn is_aggregation_detects_group_by_and_aggregate_projection() {
        let grouped = PassantSelect {
            projection: vec![ProjectionItem {
                expr: ExprRef::new("id"),
                alias: None,
            }],
            from: vec![],
            where_clause: None,
            having: None,
            group_by: vec![ExprRef::new("id")],
            order_by: vec![],
            limit: None,
            ctes: vec![],
            is_distinct: false,
            raw_sql: String::new(),
        };
        assert!(grouped.is_aggregation());

        let aggregate_projection = PassantSelect {
            projection: vec![ProjectionItem {
                expr: ExprRef::new("max(id)"),
                alias: None,
            }],
            from: vec![],
            where_clause: None,
            having: None,
            group_by: vec![],
            order_by: vec![],
            limit: None,
            ctes: vec![],
            is_distinct: false,
            raw_sql: String::new(),
        };
        assert!(aggregate_projection.is_aggregation());
    }
}
