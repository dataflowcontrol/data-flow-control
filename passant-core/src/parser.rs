use sqlparser::ast::{
    Assignment as SqlAssignment, Expr, JoinConstraint, JoinOperator, Query, Select, SelectItem,
    SetExpr, Statement, TableAlias, TableFactor, TableWithJoins,
};
use sqlparser::dialect::DuckDbDialect;
use sqlparser::parser::Parser;
use thiserror::Error;

use crate::ir::{
    Assignment, ExprRef, FromItem, JoinRef, PassantSelect, ProjectionItem, QueryIr, TableRef,
};

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("SQL parse error: {0}")]
    Sql(#[from] sqlparser::parser::ParserError),
    #[error("expected exactly one SQL statement")]
    ExpectedSingleStatement,
    #[error("unsupported query form: {0}")]
    Unsupported(String),
}

#[derive(Debug, Clone)]
pub struct ParseArtifact {
    pub statement: Statement,
    pub ir: QueryIr,
}

pub fn parse_query(sql: &str) -> Result<Statement, ParseError> {
    let dialect = DuckDbDialect {};
    let mut statements = Parser::parse_sql(&dialect, sql)?;
    if statements.len() != 1 {
        return Err(ParseError::ExpectedSingleStatement);
    }
    Ok(statements.remove(0))
}

pub fn parse_query_to_ir(sql: &str) -> Result<QueryIr, ParseError> {
    let statement = parse_query(sql)?;
    lower_statement(statement, sql)
}

impl ParseArtifact {
    pub fn from_sql(sql: &str) -> Result<Self, ParseError> {
        let statement = parse_query(sql)?;
        let ir = lower_statement(statement.clone(), sql)?;
        Ok(Self { statement, ir })
    }
}

fn lower_statement(statement: Statement, raw_sql: &str) -> Result<QueryIr, ParseError> {
    match statement {
        Statement::Query(query) => lower_query(*query, raw_sql),
        Statement::Insert(insert) => {
            let source = insert
                .source
                .ok_or_else(|| ParseError::Unsupported("insert without source query".into()))?;
            let source_ir = lower_query(*source, raw_sql)?;
            let select = match source_ir {
                QueryIr::Select(select) => select,
                _ => {
                    return Ok(QueryIr::Passthrough {
                        statement_type: "insert".to_string(),
                        raw_sql: raw_sql.to_string(),
                    });
                }
            };
            Ok(QueryIr::InsertSelect {
                sink: TableRef {
                    name: insert.table_name.to_string(),
                    alias: None,
                },
                columns: insert.columns.into_iter().map(|c| c.value).collect(),
                select: Box::new(select),
                raw_sql: raw_sql.to_string(),
            })
        }
        Statement::Update {
            table,
            assignments,
            from,
            selection,
            ..
        } => {
            let from_items = from
                .into_iter()
                .map(lower_table_with_joins)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(QueryIr::Update {
                sink: lower_table_factor(table.relation)?,
                assignments: assignments.into_iter().map(lower_assignment).collect(),
                from: from_items,
                where_clause: selection.map(expr_to_ref),
                raw_sql: raw_sql.to_string(),
            })
        }
        other => Ok(QueryIr::Passthrough {
            statement_type: statement_kind(&other).to_string(),
            raw_sql: raw_sql.to_string(),
        }),
    }
}

fn lower_query(query: Query, raw_sql: &str) -> Result<QueryIr, ParseError> {
    let with = query
        .with
        .map(|with| {
            with.cte_tables
                .into_iter()
                .map(|cte| cte.alias.name.value)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let body = match *query.body {
        SetExpr::Select(select) => *select,
        other => {
            return Ok(QueryIr::Passthrough {
                statement_type: format!("query::{other:?}"),
                raw_sql: raw_sql.to_string(),
            });
        }
    };

    let order_by = query
        .order_by
        .into_iter()
        .map(|expr| ExprRef::new(expr.to_string()))
        .collect();
    let limit = query.limit.map(expr_to_ref);

    Ok(QueryIr::Select(lower_select(
        body, order_by, limit, with, raw_sql,
    )?))
}

fn lower_select(
    select: Select,
    order_by: Vec<ExprRef>,
    limit: Option<ExprRef>,
    ctes: Vec<String>,
    raw_sql: &str,
) -> Result<PassantSelect, ParseError> {
    let from = select
        .from
        .into_iter()
        .map(lower_table_with_joins)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(PassantSelect {
        projection: select
            .projection
            .into_iter()
            .map(lower_projection)
            .collect(),
        from,
        where_clause: select.selection.map(expr_to_ref),
        having: select.having.map(expr_to_ref),
        group_by: match select.group_by {
            sqlparser::ast::GroupByExpr::Expressions(exprs, _) => {
                exprs.into_iter().map(expr_to_ref).collect()
            }
            _ => Vec::new(),
        },
        order_by,
        limit,
        ctes,
        is_distinct: select.distinct.is_some(),
        raw_sql: raw_sql.to_string(),
    })
}

fn lower_projection(item: SelectItem) -> ProjectionItem {
    match item {
        SelectItem::UnnamedExpr(expr) => ProjectionItem {
            expr: expr_to_ref(expr),
            alias: None,
        },
        SelectItem::ExprWithAlias { expr, alias } => ProjectionItem {
            expr: expr_to_ref(expr),
            alias: Some(alias.value),
        },
        other => ProjectionItem {
            expr: ExprRef::new(other.to_string()),
            alias: None,
        },
    }
}

fn lower_table_with_joins(table: TableWithJoins) -> Result<FromItem, ParseError> {
    let base = lower_table_factor(table.relation)?;
    let joins = table
        .joins
        .into_iter()
        .map(|join| JoinRef {
            relation_sql: join.relation.to_string(),
            condition_sql: join_constraint_sql(&join.join_operator),
        })
        .collect::<Vec<_>>();

    Ok(FromItem {
        relation_sql: base.name.clone(),
        alias: base.alias.clone(),
        tables: vec![base],
        joins,
    })
}

fn lower_table_factor(factor: TableFactor) -> Result<TableRef, ParseError> {
    match factor {
        TableFactor::Table { name, alias, .. } => Ok(TableRef {
            name: name.to_string(),
            alias: alias.map(alias_to_name),
        }),
        TableFactor::Derived {
            alias, subquery, ..
        } => Ok(TableRef {
            name: format!("({subquery})"),
            alias: alias.map(alias_to_name),
        }),
        other => Err(ParseError::Unsupported(format!("table factor {other:?}"))),
    }
}

fn lower_assignment(assignment: SqlAssignment) -> Assignment {
    Assignment {
        column: assignment.target.to_string(),
        value: expr_to_ref(assignment.value),
    }
}

fn expr_to_ref(expr: Expr) -> ExprRef {
    ExprRef::new(expr.to_string())
}

fn alias_to_name(alias: TableAlias) -> String {
    alias.name.value
}

fn join_constraint_sql(operator: &JoinOperator) -> Option<String> {
    let constraint = match operator {
        JoinOperator::Inner(constraint)
        | JoinOperator::LeftOuter(constraint)
        | JoinOperator::RightOuter(constraint)
        | JoinOperator::FullOuter(constraint)
        | JoinOperator::Semi(constraint)
        | JoinOperator::LeftSemi(constraint)
        | JoinOperator::RightSemi(constraint)
        | JoinOperator::Anti(constraint)
        | JoinOperator::LeftAnti(constraint)
        | JoinOperator::RightAnti(constraint) => constraint,
        JoinOperator::AsOf { constraint, .. } => constraint,
        JoinOperator::CrossJoin | JoinOperator::CrossApply | JoinOperator::OuterApply => {
            return None;
        }
    };

    match constraint {
        JoinConstraint::On(expr) => Some(expr.to_string()),
        JoinConstraint::Using(columns) => Some(format!(
            "USING ({})",
            columns
                .iter()
                .map(|ident| ident.value.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        JoinConstraint::Natural => Some("NATURAL".to_string()),
        JoinConstraint::None => None,
    }
}

fn statement_kind(statement: &Statement) -> &'static str {
    match statement {
        Statement::CreateTable(_) => "create_table",
        Statement::Delete(_) => "delete",
        Statement::Drop { .. } => "drop",
        Statement::Explain { .. } => "explain",
        Statement::Query(_) => "query",
        Statement::Update { .. } => "update",
        Statement::Insert(_) => "insert",
        _ => "other",
    }
}
