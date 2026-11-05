//! Native planning for trusted SQL models of fixed external tool payloads.
//!
//! The host supplies catalog facts and bound values, executes the returned SQL
//! in one transaction, and never interprets policy or correspondence semantics.
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use serde::Serialize;
use sqlparser::ast::{
    Expr, GroupByExpr, Ident, JoinConstraint, JoinOperator, Query, Select, SelectItem, SetExpr,
    Statement, TableFactor, TableWithJoins, Value, Visit, VisitMut, Visitor, VisitorMut,
};
use sqlparser::dialect::DuckDbDialect;
use sqlparser::tokenizer::{Token, Tokenizer};

use crate::{AggregateRegistry, PassantRewriter, RewriteError, RewriteOptions, TableCatalog};

fn invalid(message: impl Into<String>) -> RewriteError {
    RewriteError::unsupported_statement(message)
}
fn ident(value: &str) -> Result<String, RewriteError> {
    if value.is_empty() {
        return Err(invalid("SQL identifiers must be nonempty strings"));
    }
    Ok(Ident::with_quote('"', value).to_string())
}

#[derive(Debug, Clone)]
pub struct ToolProvenanceModel {
    pub query: Query,
    pub key_columns: Vec<String>,
    pub parameter_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolPublicationPlan {
    pub model_keys_sql: String,
    pub correspondence_sql: Option<String>,
    pub validation_sql: String,
    pub publication_sql: Option<String>,
    pub sink_kill_preflight_sql: Option<String>,
    pub parameter_count: usize,
    pub model_parameter_count: usize,
    pub sink_preflight_binding_indices: Vec<usize>,
    pub output_columns: Vec<String>,
}

pub(crate) struct ToolPublicationQueries {
    pub model: Query,
    pub insert: Option<Statement>,
    pub sink_candidates: Option<Statement>,
    pub model_keys_sql: String,
    pub correspondence_sql: Option<String>,
    pub validation_sql: String,
    pub parameter_count: usize,
    pub sink_preflight_binding_indices: Vec<usize>,
}

impl ToolProvenanceModel {
    pub fn new(sql: &str, key_columns: Vec<String>) -> Result<Self, RewriteError> {
        let sql = normalize_tokens(sql)?;
        let statement = crate::parser::parse_query(&sql)?;
        let Statement::Query(query) = statement else {
            return Err(invalid("Tool provenance requires one SELECT query"));
        };
        struct Validate {
            parameter_count: usize,
            error: Option<String>,
        }
        impl Visitor for Validate {
            type Break = ();
            fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<()> {
                if validate_read_only_body(&query.body).is_err() || !query.locks.is_empty() {
                    self.error = Some("Tool provenance requires a read-only query".into());
                }
                ControlFlow::Continue(())
            }
            fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
                match expr {
                    Expr::Value(Value::Placeholder(name)) if name == "?" => {
                        self.parameter_count += 1
                    }
                    Expr::Value(Value::Placeholder(_)) => {
                        self.error = Some("Tool models support positional ? parameters only".into())
                    }
                    Expr::Function(function)
                        if matches!(
                            function.name.to_string().to_ascii_lowercase().as_str(),
                            "random" | "rand" | "uuid" | "gen_random_uuid"
                        ) =>
                    {
                        self.error = Some(
                            "Tool provenance must be deterministic; bind observed values instead"
                                .into(),
                        );
                    }
                    _ => {}
                }
                ControlFlow::Continue(())
            }
            fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<()> {
                if !matches!(statement, Statement::Query(_)) {
                    self.error = Some("Tool provenance requires a read-only query".into());
                }
                ControlFlow::Continue(())
            }
        }
        let mut validate = Validate {
            parameter_count: 0,
            error: None,
        };
        let _ = query.visit(&mut validate);
        if let Some(error) = validate.error {
            return Err(invalid(error));
        }
        validate_read_only_body(&query.body)?;
        let mut keys = HashSet::new();
        for column in &key_columns {
            ident(column)?;
            if !keys.insert(column.clone()) {
                return Err(invalid("Modeled key columns must be distinct"));
            }
        }
        Ok(Self {
            query: *query,
            key_columns,
            parameter_count: validate.parameter_count,
        })
    }

    pub fn sql(&self) -> String {
        self.query.to_string()
    }

    pub fn validate_parameter_count(&self, supplied: usize) -> Result<(), RewriteError> {
        if supplied != self.parameter_count {
            return Err(invalid(
                "Tool model parameter count does not match positional ? parameters",
            ));
        }
        Ok(())
    }

    pub fn validate_output_shapes(
        &self,
        key_arities: &[usize],
        row_columns: &[Vec<String>],
    ) -> Result<Vec<String>, RewriteError> {
        if key_arities.len() != row_columns.len()
            || key_arities
                .iter()
                .any(|size| *size != self.key_columns.len())
        {
            return Err(invalid("Output key arity does not match key_columns"));
        }
        let Some(first) = row_columns.first() else {
            return Ok(Vec::new());
        };
        if first.is_empty() {
            return Err(invalid("Actual output rows must have at least one column"));
        }
        let expected: HashSet<_> = first.iter().collect();
        if expected.len() != first.len() {
            return Err(invalid("Actual output columns must be distinct"));
        }
        for columns in row_columns {
            for column in columns {
                ident(column)?;
            }
            if columns.len() != first.len() || columns.iter().collect::<HashSet<_>>() != expected {
                return Err(invalid(
                    "Actual output rows must have identical column names",
                ));
            }
        }
        Ok(first.clone())
    }

    pub fn validate_correspondence_status(status: i64) -> Result<(), RewriteError> {
        let message = match status {
            0 => return Ok(()),
            1 => "Actual output keys must not contain NULL or NaN",
            2 => {
                "Actual output keys must be unique under SQL equality; distinguish duplicate occurrences"
            }
            3 => "Modeled output keys must not contain NULL or NaN",
            4 => {
                "Modeled output keys must be unique under SQL equality; distinguish duplicate occurrences"
            }
            5 => "Actual outputs do not match all modeled output keys",
            6 => "Output keys are ambiguous under SQL equality",
            _ => "Invalid native tool correspondence status",
        };
        Err(invalid(message))
    }

    pub(crate) fn prepare(
        &self,
        catalog: &TableCatalog,
        registry: &AggregateRegistry,
        sink: &str,
        columns: &[String],
        row_count: usize,
    ) -> Result<ToolPublicationQueries, RewriteError> {
        let target = ident(sink)?;
        for column in columns {
            ident(column)?;
        }
        if row_count != 0 && columns.is_empty() {
            return Err(invalid("Actual output rows must have at least one column"));
        }
        let mut model = self.query.clone();
        expand_query(&mut model, catalog, &HashMap::new())?;
        number_parameters(&mut model);
        let names = identifier_names(&model.to_string())?;
        let model_alias = fresh_name("__dfc_tool_model", &names);
        let actual_alias = fresh_name("__dfc_tool_actual", &names);
        let key_projection = if self.key_columns.is_empty() {
            "1".into()
        } else {
            self.key_columns
                .iter()
                .map(|name| ident(name))
                .collect::<Result<Vec<_>, _>>()?
                .join(", ")
        };
        let model_keys_sql = format!("SELECT {key_projection} FROM ({model}) AS {model_alias}");
        let model_bad = invalid_keys("m", &self.key_columns);
        let model_duplicate = duplicate_keys("m", &self.key_columns)?;
        if row_count == 0 {
            let validation_sql = format!(
                "WITH m AS ({model}) SELECT CASE WHEN {model_bad} THEN 3 WHEN {model_duplicate} THEN 4 WHEN (SELECT COUNT(*) FROM m) <> 0 THEN 5 ELSE 0 END"
            );
            return Ok(ToolPublicationQueries {
                model,
                insert: None,
                sink_candidates: None,
                model_keys_sql,
                correspondence_sql: None,
                validation_sql,
                parameter_count: self.parameter_count,
                sink_preflight_binding_indices: Vec::new(),
            });
        }
        let key_names: Vec<_> = (0..self.key_columns.len())
            .map(|i| format!("k{i}"))
            .collect();
        let value_names: Vec<_> = (0..columns.len()).map(|i| format!("v{i}")).collect();
        let aliases = key_names
            .iter()
            .chain(&value_names)
            .cloned()
            .collect::<Vec<_>>();
        let mut next = self.parameter_count;
        let values = (0..row_count)
            .map(|_| {
                let row = aliases
                    .iter()
                    .map(|_| {
                        next += 1;
                        format!("${next}")
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({row})")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let actual = format!(
            "(VALUES {values}) AS {actual_alias}({})",
            aliases.join(", ")
        );
        let predicate = key_predicate(&model_alias, &self.key_columns, &actual_alias, &key_names)?;
        let source = format!("({model}) AS {model_alias} JOIN {actual} ON {predicate}");
        let correspondence_sql = Some(format!("SELECT COUNT(*) FROM {source}"));
        let projection = value_names
            .iter()
            .map(|name| format!("{actual_alias}.{name}"))
            .collect::<Vec<_>>()
            .join(", ");
        let target_columns = columns
            .iter()
            .map(|name| ident(name))
            .collect::<Result<Vec<_>, _>>()?
            .join(", ");
        let sql =
            format!("INSERT INTO {target} ({target_columns}) SELECT {projection} FROM {source}");
        let mut insert = crate::parser::parse_query(&sql)?;
        flatten_binding(&mut insert, &model, &model_alias, &actual_alias, registry)?;
        let sink_preflight_binding_indices = (0..row_count)
            .flat_map(|row| {
                let first = self.parameter_count + row * aliases.len() + key_names.len();
                (0..columns.len()).map(move |column| first + column)
            })
            .collect::<Vec<_>>();
        let candidates = (0..row_count)
            .map(|row| {
                format!(
                    "SELECT {}",
                    (0..columns.len())
                        .map(|column| format!("${}", row * columns.len() + column + 1))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        let sink_candidates = crate::parser::parse_query(&format!(
            "INSERT INTO {target} ({target_columns}) {candidates}"
        ))?;
        let actual_bad = invalid_keys("a", &key_names);
        let actual_duplicate = duplicate_keys("a", &key_names)?;
        let match_predicate = key_predicate("m", &self.key_columns, "a", &key_names)?;
        let validation_sql = format!(
            "WITH m AS ({model}), a({}) AS (VALUES {values}) SELECT CASE WHEN {actual_bad} THEN 1 WHEN {actual_duplicate} THEN 2 WHEN {model_bad} THEN 3 WHEN {model_duplicate} THEN 4 WHEN (SELECT COUNT(*) FROM m) <> {row_count} OR EXISTS (SELECT 1 FROM m WHERE NOT EXISTS (SELECT 1 FROM a WHERE {match_predicate})) THEN 5 WHEN (SELECT COUNT(*) FROM m JOIN a ON {match_predicate}) <> {row_count} THEN 6 ELSE 0 END",
            aliases.join(", ")
        );
        Ok(ToolPublicationQueries {
            model,
            insert: Some(insert),
            sink_candidates: Some(sink_candidates),
            model_keys_sql,
            correspondence_sql,
            validation_sql,
            parameter_count: next,
            sink_preflight_binding_indices,
        })
    }
}

fn validate_read_only_body(body: &SetExpr) -> Result<(), RewriteError> {
    match body {
        SetExpr::Select(select) if select.into.is_some() => {
            Err(invalid("Tool provenance requires a read-only query"))
        }
        SetExpr::Select(_) | SetExpr::Values(_) | SetExpr::Table(_) => Ok(()),
        SetExpr::Query(query) => validate_read_only_body(&query.body),
        SetExpr::SetOperation { left, right, .. } => {
            validate_read_only_body(left)?;
            validate_read_only_body(right)
        }
        _ => Err(invalid("Tool provenance requires a read-only query")),
    }
}

fn normalize_tokens(sql: &str) -> Result<String, RewriteError> {
    let tokens = Tokenizer::new(&DuckDbDialect {}, sql)
        .tokenize()
        .map_err(|e| invalid(e.to_string()))?;
    let significant = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    let mut frame_scopes: Vec<bool> = Vec::new();
    for (index, token) in significant.iter().enumerate() {
        match token {
            Token::LParen => frame_scopes.push(false),
            Token::RParen => {
                frame_scopes.pop();
            }
            Token::Word(word) if word.quote_style.is_none() => {
                let word = word.value.to_ascii_uppercase();
                if matches!(word.as_str(), "ROWS" | "RANGE" | "GROUPS")
                    && let Some(scope) = frame_scopes.last_mut()
                {
                    *scope = true;
                }
                if word == "EXCLUDE"
                    && frame_scopes.iter().any(|scope| *scope)
                    && significant.get(index + 1).is_some_and(|next| {
                        matches!(
                            next.to_string().to_ascii_uppercase().as_str(),
                            "CURRENT" | "GROUP" | "TIES" | "NO"
                        )
                    })
                {
                    return Err(invalid(
                        "Tool provenance does not support window frame EXCLUDE clauses",
                    ));
                }
            }
            _ => {}
        }
    }
    let mut out = String::new();
    for (index, token) in tokens.iter().enumerate() {
        let next = tokens[index + 1..]
            .iter()
            .find(|token| !matches!(token, Token::Whitespace(_)));
        if matches!(token, Token::Comma)
            && next.is_some_and(|token| token.to_string().eq_ignore_ascii_case("LATERAL"))
        {
            out.push_str(" CROSS JOIN ");
        } else if matches!(token,Token::Word(word) if word.quote_style.is_none() && word.value.eq_ignore_ascii_case("LISTAGG"))
            && matches!(next, Some(Token::LParen))
        {
            out.push_str("STRING_AGG");
        } else {
            out.push_str(&token.to_string());
        }
    }
    Ok(out)
}

fn number_parameters(query: &mut Query) {
    struct Number(usize);
    impl VisitorMut for Number {
        type Break = ();
        fn pre_visit_expr(&mut self, expression: &mut Expr) -> ControlFlow<()> {
            if let Expr::Value(Value::Placeholder(name)) = expression {
                self.0 += 1;
                *name = format!("${}", self.0);
            }
            ControlFlow::Continue(())
        }
    }
    let _ = query.visit(&mut Number(0));
}

fn invalid_keys(table: &str, columns: &[String]) -> String {
    if columns.is_empty() {
        return "FALSE".into();
    }
    let predicates = columns.iter().map(|column| {
        let column = format!("{table}.{}", Ident::with_quote('"',column));
        format!("{column} IS NULL OR (typeof({column}) IN ('FLOAT', 'DOUBLE') AND isnan(TRY_CAST({column} AS DOUBLE)))")
    }).collect::<Vec<_>>().join(" OR ");
    format!("EXISTS (SELECT 1 FROM {table} WHERE {predicates})")
}
fn duplicate_keys(table: &str, columns: &[String]) -> Result<String, RewriteError> {
    if columns.is_empty() {
        return Ok(format!("(SELECT COUNT(*) FROM {table}) > 1"));
    }
    let keys = columns
        .iter()
        .map(|name| ident(name))
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    Ok(format!(
        "EXISTS (SELECT 1 FROM {table} GROUP BY {keys} HAVING COUNT(*) > 1)"
    ))
}
fn key_predicate(
    left: &str,
    left_columns: &[String],
    right: &str,
    right_columns: &[String],
) -> Result<String, RewriteError> {
    if left_columns.is_empty() {
        return Ok("TRUE".into());
    }
    left_columns
        .iter()
        .zip(right_columns)
        .map(|(a, b)| {
            Ok(format!(
                "{left}.{} IS NOT DISTINCT FROM {right}.{}",
                ident(a)?,
                ident(b)?
            ))
        })
        .collect::<Result<Vec<_>, RewriteError>>()
        .map(|x| x.join(" AND "))
}
fn identifier_names(sql: &str) -> Result<HashSet<String>, RewriteError> {
    Ok(Tokenizer::new(&DuckDbDialect {}, sql)
        .tokenize()
        .map_err(|e| invalid(e.to_string()))?
        .into_iter()
        .filter_map(|token| match token {
            Token::Word(word) => Some(word.value.to_ascii_lowercase()),
            _ => None,
        })
        .collect())
}
fn fresh_name(base: &str, names: &HashSet<String>) -> String {
    let mut result = base.to_string();
    let mut n = 0;
    while names.contains(&result.to_ascii_lowercase()) {
        n += 1;
        result = format!("{base}_{n}");
    }
    result
}

#[derive(Clone)]
struct InputColumn {
    qualifier: Vec<Ident>,
    name: String,
}
fn output_columns(query: &Query) -> Vec<String> {
    match query.body.as_ref() {
        SetExpr::Select(select) => select
            .projection
            .iter()
            .filter_map(|item| match item {
                SelectItem::ExprWithAlias { alias, .. } => Some(alias.value.clone()),
                SelectItem::UnnamedExpr(Expr::Identifier(id)) => Some(id.value.clone()),
                SelectItem::UnnamedExpr(Expr::CompoundIdentifier(ids)) => {
                    ids.last().map(|id| id.value.clone())
                }
                SelectItem::UnnamedExpr(expr) => Some(expr.to_string()),
                _ => None,
            })
            .collect(),
        SetExpr::Query(query) => output_columns(query),
        SetExpr::SetOperation { left, .. } => {
            let mut query = crate::sql::query_from_select(crate::sql::empty_select());
            query.body = left.clone();
            output_columns(&query)
        }
        _ => Vec::new(),
    }
}
fn expand_query(
    query: &mut Query,
    catalog: &TableCatalog,
    inherited: &HashMap<String, Vec<String>>,
) -> Result<(), RewriteError> {
    let mut ctes = inherited.clone();
    if let Some(with) = &mut query.with {
        for cte in &mut with.cte_tables {
            expand_query(&mut cte.query, catalog, &ctes)?;
            let names = if cte.alias.columns.is_empty() {
                output_columns(&cte.query)
            } else {
                cte.alias
                    .columns
                    .iter()
                    .map(|column| column.name.value.clone())
                    .collect()
            };
            ctes.insert(cte.alias.name.value.to_ascii_lowercase(), names);
        }
    }
    expand_body(&mut query.body, catalog, &ctes)
}
fn expand_body(
    body: &mut SetExpr,
    catalog: &TableCatalog,
    ctes: &HashMap<String, Vec<String>>,
) -> Result<(), RewriteError> {
    match body {
        SetExpr::Select(select) => expand_select(select, catalog, ctes),
        SetExpr::Query(query) => expand_query(query, catalog, ctes),
        SetExpr::SetOperation { left, right, .. } => {
            expand_body(left, catalog, ctes)?;
            expand_body(right, catalog, ctes)
        }
        _ => Ok(()),
    }
}
fn factor_columns(
    factor: &mut TableFactor,
    catalog: &TableCatalog,
    ctes: &HashMap<String, Vec<String>>,
) -> Result<Vec<InputColumn>, RewriteError> {
    let (qualifier, mut names, alias) = match factor {
        TableFactor::Table { name, alias, .. } => {
            let names = ctes
                .get(&name.to_string().to_ascii_lowercase())
                .cloned()
                .or_else(|| catalog.columns(&name.to_string()).map(|x| x.to_vec()))
                .unwrap_or_default();
            (name.0.clone(), names, alias.as_ref())
        }
        TableFactor::Derived {
            subquery, alias, ..
        } => {
            expand_query(subquery, catalog, ctes)?;
            (Vec::new(), output_columns(subquery), alias.as_ref())
        }
        _ => return Ok(Vec::new()),
    };
    let qualifier = if let Some(alias) = alias {
        if !alias.columns.is_empty() {
            names = alias
                .columns
                .iter()
                .map(|column| column.name.value.clone())
                .collect();
        }
        vec![alias.name.clone()]
    } else {
        qualifier
    };
    Ok(names
        .into_iter()
        .map(|name| InputColumn {
            qualifier: qualifier.clone(),
            name,
        })
        .collect())
}
fn expand_select(
    select: &mut Select,
    catalog: &TableCatalog,
    ctes: &HashMap<String, Vec<String>>,
) -> Result<(), RewriteError> {
    let mut inputs = Vec::new();
    for table in &mut select.from {
        inputs.extend(factor_columns(&mut table.relation, catalog, ctes)?);
        for join in &mut table.joins {
            inputs.extend(factor_columns(&mut join.relation, catalog, ctes)?);
        }
    }
    let mut projection = Vec::new();
    for item in std::mem::take(&mut select.projection) {
        let qualifier = match &item {
            SelectItem::Wildcard(options) if options == &Default::default() => Some(None),
            SelectItem::QualifiedWildcard(name, options) if options == &Default::default() => {
                Some(Some(name))
            }
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                return Err(invalid("Tool model wildcard modifiers are unsupported"));
            }
            _ => None,
        };
        if let Some(qualifier) = qualifier {
            let mut count = 0;
            for input in &inputs {
                if qualifier.is_none_or(|name| name.0 == input.qualifier) {
                    let mut ids = input.qualifier.clone();
                    ids.push(Ident::with_quote('"', &input.name));
                    projection.push(SelectItem::UnnamedExpr(Expr::CompoundIdentifier(ids)));
                    count += 1;
                }
            }
            if count == 0 {
                return Err(invalid(
                    "Cannot expand tool model wildcard without catalog columns",
                ));
            }
        } else {
            projection.push(item);
        }
    }
    select.projection = projection;
    struct Qualify<'a>(&'a [InputColumn]);
    impl VisitorMut for Qualify<'_> {
        type Break = ();
        fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<()> {
            if let Expr::Identifier(id) = expr {
                let matches = self
                    .0
                    .iter()
                    .filter(|column| column.name.eq_ignore_ascii_case(&id.value))
                    .collect::<Vec<_>>();
                if matches.len() == 1 {
                    let mut ids = matches[0].qualifier.clone();
                    ids.push(id.clone());
                    *expr = Expr::CompoundIdentifier(ids);
                }
            }
            ControlFlow::Continue(())
        }
        fn pre_visit_query(&mut self, _: &mut Query) -> ControlFlow<()> {
            ControlFlow::Break(())
        }
    }
    let _ = select.visit(&mut Qualify(&inputs));
    Ok(())
}

fn flatten_binding(
    statement: &mut Statement,
    model: &Query,
    model_alias: &str,
    actual_alias: &str,
    registry: &AggregateRegistry,
) -> Result<(), RewriteError> {
    let SetExpr::Select(inner) = model.body.as_ref() else {
        return Ok(());
    };
    if model.with.is_some()
        || model.order_by.is_some()
        || model.limit.is_some()
        || model.offset.is_some()
        || model.fetch.is_some()
        || inner.distinct.is_some()
        || inner.having.is_some()
        || inner.qualify.is_some()
        || !matches!(&inner.group_by,GroupByExpr::Expressions(items,_) if items.is_empty())
    {
        return Ok(());
    }
    let mut unsafe_expr = false;
    let _: ControlFlow<()> = sqlparser::ast::visit_expressions(inner.as_ref(), |expr| {
        if matches!(
            expr,
            Expr::Subquery(_) | Expr::Exists { .. } | Expr::Function(_)
        ) || registry.expr_contains_aggregate(expr)
        {
            unsafe_expr = true;
        }
        ControlFlow::Continue(())
    });
    if unsafe_expr {
        return Ok(());
    }
    let Statement::Insert(insert) = statement else {
        return Ok(());
    };
    let Some(source) = &mut insert.source else {
        return Ok(());
    };
    let SetExpr::Select(outer) = source.body.as_mut() else {
        return Ok(());
    };
    let Some(join) = outer
        .from
        .first()
        .and_then(|table| table.joins.first())
        .cloned()
    else {
        return Ok(());
    };
    let mut mapping = HashMap::new();
    for item in &inner.projection {
        let (name, expr) = match item {
            SelectItem::ExprWithAlias { expr, alias } => (alias.value.clone(), expr.clone()),
            SelectItem::UnnamedExpr(Expr::CompoundIdentifier(ids)) => (
                ids.last().unwrap().value.clone(),
                Expr::CompoundIdentifier(ids.clone()),
            ),
            SelectItem::UnnamedExpr(Expr::Identifier(id)) => {
                (id.value.clone(), Expr::Identifier(id.clone()))
            }
            _ => continue,
        };
        if mapping.insert(name.to_ascii_lowercase(), expr).is_some() {
            return Ok(());
        }
    }
    let mut predicate = match join.join_operator {
        JoinOperator::Inner(JoinConstraint::On(predicate)) => predicate,
        _ => return Ok(()),
    };
    let _ = sqlparser::ast::visit_expressions_mut(&mut predicate, |expr| {
        if let Expr::CompoundIdentifier(ids) = expr
            && ids.len() == 2
            && ids[0].value.eq_ignore_ascii_case(model_alias)
            && let Some(replacement) = mapping.get(&ids[1].value.to_ascii_lowercase())
        {
            *expr = replacement.clone();
        }
        ControlFlow::<()>::Continue(())
    });
    let mut from = inner.from.clone();
    if from.is_empty() {
        return Ok(());
    }
    // The original FROM cross-product keeps its order; the occurrence relation
    // joins the complete FROM scope, with the key predicate in WHERE.
    from.push(TableWithJoins {
        relation: join.relation,
        joins: Vec::new(),
    });
    outer.from = from;
    outer.selection = Some(match &inner.selection {
        Some(selection) => crate::sql::and_exprs(vec![selection.clone(), predicate]).unwrap(),
        None => predicate,
    });
    let _ = actual_alias;
    Ok(())
}

impl ToolPublicationPlan {
    pub fn check_validation(status: i64) -> Result<(), RewriteError> {
        ToolProvenanceModel::validate_correspondence_status(status)
    }
}

impl PassantRewriter {
    pub fn plan_tool_publication(
        &self,
        model: &ToolProvenanceModel,
        sink: &str,
        key_arities: &[usize],
        row_columns: &[Vec<String>],
        supplied_model_parameter_count: usize,
    ) -> Result<ToolPublicationPlan, RewriteError> {
        model.validate_parameter_count(supplied_model_parameter_count)?;
        let output_columns = model.validate_output_shapes(key_arities, row_columns)?;
        self.validate_tool_model_policy_shape(&model.query)?;
        let prepared = model.prepare(
            &self.catalog,
            &self.aggregate_registry,
            sink,
            &output_columns,
            row_columns.len(),
        )?;
        let (publication_sql, sink_kill_preflight_sql) = if let (Some(insert), Some(candidates)) =
            (&prepared.insert, &prepared.sink_candidates)
        {
            let enforced = self.rewrite_tool_publication(
                &prepared.model,
                insert,
                candidates,
                RewriteOptions::default(),
            )?;
            (
                Some(enforced.publication_sql),
                enforced.sink_kill_preflight_sql,
            )
        } else {
            (None, None)
        };
        Ok(ToolPublicationPlan {
            model_keys_sql: prepared.model_keys_sql,
            correspondence_sql: prepared.correspondence_sql,
            validation_sql: prepared.validation_sql,
            publication_sql,
            sink_kill_preflight_sql,
            parameter_count: prepared.parameter_count,
            model_parameter_count: model.parameter_count,
            sink_preflight_binding_indices: prepared.sink_preflight_binding_indices,
            output_columns,
        })
    }
}
