//! Substitute policy sink fields without losing SQL subquery scopes.
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Query, Select, SetExpr, TableFactor,
    TableWithJoins, Visit, VisitMut, Visitor, VisitorMut,
};

use crate::diagnostics::{ErrorKind, RewriteError};
use crate::identifiers::{QualifiedColumn, TableName};

pub(super) fn replace(
    mut expr: Expr,
    sink: &str,
    columns: &HashMap<String, Expr>,
    ambiguous: Option<&HashSet<String>>,
) -> Result<Expr, RewriteError> {
    let replacement = Replacement {
        sink: TableName::parse(sink),
        columns,
        ambiguous,
    };
    struct References<'a>(&'a TableName);
    impl Visitor for References<'_> {
        type Break = ();
        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
            if let Expr::CompoundIdentifier(parts) = expr
                && QualifiedColumn::from_compound_identifier(parts)
                    .is_some_and(|column| self.0.matches_name(column.table.as_str()))
            {
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        }
    }
    if matches!(
        expr.visit(&mut References(&replacement.sink)),
        ControlFlow::Continue(())
    ) {
        return Ok(expr);
    }
    replacement.outer_expression(&mut expr)?;
    Ok(expr)
}

struct Replacement<'a> {
    sink: TableName,
    columns: &'a HashMap<String, Expr>,
    ambiguous: Option<&'a HashSet<String>>,
}

impl Replacement<'_> {
    // Keep existing non-subquery substitution behavior. This repair expands
    // traversal only where a constraint introduces a subquery, not unrelated
    // policy expression forms (which have separate rewrite expectations).
    fn outer_expression(&self, expr: &mut Expr) -> Result<(), RewriteError> {
        match expr {
            Expr::CompoundIdentifier(_) => self.expression(expr, &HashSet::new())?,
            Expr::BinaryOp { left, right, .. } => {
                self.outer_expression(left)?;
                self.outer_expression(right)?;
            }
            Expr::Nested(expr)
            | Expr::UnaryOp { expr, .. }
            | Expr::IsNull(expr)
            | Expr::IsNotNull(expr) => self.outer_expression(expr)?,
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => {
                if let Some(expr) = operand {
                    self.outer_expression(expr)?;
                }
                for expr in conditions.iter_mut().chain(results.iter_mut()) {
                    self.outer_expression(expr)?;
                }
                if let Some(expr) = else_result {
                    self.outer_expression(expr)?;
                }
            }
            Expr::Function(function) => {
                if let FunctionArguments::List(args) = &mut function.args {
                    for arg in &mut args.args {
                        match arg {
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                            | FunctionArg::Named {
                                arg: FunctionArgExpr::Expr(expr),
                                ..
                            }
                            | FunctionArg::ExprNamed {
                                arg: FunctionArgExpr::Expr(expr),
                                ..
                            } => self.outer_expression(expr)?,
                            _ => {}
                        }
                    }
                    self.walk_queries(&mut args.clauses)?;
                } else {
                    self.walk_queries(&mut function.args)?;
                }
                let args = std::mem::replace(&mut function.args, FunctionArguments::None);
                let result = self.walk_queries(function);
                function.args = args;
                result?;
            }
            Expr::Exists { .. } | Expr::Subquery(_) | Expr::InSubquery { .. } => {
                self.walk(expr, &HashSet::new())?
            }
            _ => self.walk_queries(expr)?,
        }
        Ok(())
    }

    fn walk_queries<T: VisitMut>(&self, expr: &mut T) -> Result<(), RewriteError> {
        let mut visitor = ScopedExpressions {
            replacement: self,
            locals: &HashSet::new(),
            query_depth: 0,
            replace_expressions: false,
        };
        match expr.visit(&mut visitor) {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(error) => Err(error),
        }
    }

    fn walk<T: VisitMut>(
        &self,
        node: &mut T,
        locals: &HashSet<String>,
    ) -> Result<(), RewriteError> {
        let mut visitor = ScopedExpressions {
            replacement: self,
            locals,
            query_depth: 0,
            replace_expressions: true,
        };
        match node.visit(&mut visitor) {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(error) => Err(error),
        }
    }

    fn query(&self, query: &mut Query, locals: &HashSet<String>) -> Result<(), RewriteError> {
        // CTE definitions do not see aliases declared by the main SELECT. A CTE
        // name becomes a column qualifier only when it occurs in a FROM scope.
        if let Some(with) = &mut query.with {
            for cte in &mut with.cte_tables {
                self.query(&mut cte.query, locals)?;
            }
        }
        let tail_locals = self.body(&mut query.body, locals)?;
        self.walk(&mut query.order_by, &tail_locals)?;
        self.walk(&mut query.limit, &tail_locals)?;
        self.walk(&mut query.limit_by, &tail_locals)?;
        self.walk(&mut query.offset, &tail_locals)?;
        self.walk(&mut query.fetch, &tail_locals)?;
        self.walk(&mut query.settings, &tail_locals)?;
        Ok(())
    }

    fn body(
        &self,
        body: &mut SetExpr,
        locals: &HashSet<String>,
    ) -> Result<HashSet<String>, RewriteError> {
        match body {
            SetExpr::Select(select) => self.select(select, locals),
            SetExpr::Query(query) => {
                self.query(query, locals)?;
                Ok(locals.clone())
            }
            SetExpr::SetOperation { left, right, .. } => {
                // A binding in one branch must not shadow the sibling branch.
                self.body(left, locals)?;
                self.body(right, locals)?;
                Ok(locals.clone())
            }
            SetExpr::Values(values) => {
                self.walk(values, locals)?;
                Ok(locals.clone())
            }
            SetExpr::Table(_) => Ok(locals.clone()),
            _ => Err(RewriteError::unsupported_statement(
                "write statement in policy constraint subquery",
            )),
        }
    }

    fn select(
        &self,
        select: &mut Select,
        inherited: &HashSet<String>,
    ) -> Result<HashSet<String>, RewriteError> {
        // FROM arguments see outer/prior bindings, not the alias being declared.
        // Visit them separately so `unnest(i.items) AS i(x)` keeps its meaning.
        let mut from = std::mem::take(&mut select.from);
        let mut locals = inherited.clone();
        for table in &mut from {
            self.from(table, &mut locals)?;
        }
        let result = self.walk(select, &locals);
        select.from = from;
        result?;
        Ok(locals)
    }

    fn from(
        &self,
        table: &mut TableWithJoins,
        locals: &mut HashSet<String>,
    ) -> Result<(), RewriteError> {
        self.factor(&mut table.relation, locals)?;
        for join in &mut table.joins {
            self.factor(&mut join.relation, locals)?;
            self.walk(&mut join.join_operator, locals)?;
        }
        Ok(())
    }

    fn factor(
        &self,
        factor: &mut TableFactor,
        locals: &mut HashSet<String>,
    ) -> Result<(), RewriteError> {
        if let TableFactor::NestedJoin {
            table_with_joins,
            alias,
        } = factor
        {
            let mut inner = locals.clone();
            self.from(table_with_joins, &mut inner)?;
            if let Some(alias) = alias {
                locals.insert(alias.name.value.to_ascii_lowercase());
            } else {
                *locals = inner;
            }
            return Ok(());
        }
        if matches!(
            factor,
            TableFactor::Pivot { .. }
                | TableFactor::Unpivot { .. }
                | TableFactor::MatchRecognize { .. }
        ) {
            return Err(RewriteError::unsupported_statement(
                "policy sink substitution through PIVOT, UNPIVOT or MATCH_RECOGNIZE requires operator-specific scope handling",
            ));
        }
        self.walk(factor, locals)?;
        let alias = match factor {
            TableFactor::Table { name, alias, .. } => {
                if alias.is_none() {
                    locals.insert(TableName::from_object_name(name).key());
                    if let Some(last) = name.0.last() {
                        locals.insert(last.value.to_ascii_lowercase());
                    }
                }
                alias
            }
            TableFactor::Function { name, alias, .. } => {
                if alias.is_none() {
                    locals.insert(TableName::from_object_name(name).key());
                    if let Some(last) = name.0.last() {
                        locals.insert(last.value.to_ascii_lowercase());
                    }
                }
                alias
            }
            TableFactor::UNNEST { alias, .. } => {
                if alias.is_none() {
                    locals.insert("unnest".to_string());
                }
                alias
            }
            TableFactor::Derived { alias, .. }
            | TableFactor::TableFunction { alias, .. }
            | TableFactor::JsonTable { alias, .. }
            | TableFactor::OpenJsonTable { alias, .. }
            | TableFactor::Pivot { alias, .. }
            | TableFactor::Unpivot { alias, .. }
            | TableFactor::MatchRecognize { alias, .. }
            | TableFactor::NestedJoin { alias, .. } => alias,
        };
        if let Some(alias) = alias {
            locals.insert(alias.name.value.to_ascii_lowercase());
        }
        Ok(())
    }

    fn expression(&self, expr: &mut Expr, locals: &HashSet<String>) -> Result<(), RewriteError> {
        let Expr::CompoundIdentifier(parts) = expr else {
            return Ok(());
        };
        let Some(column) = QualifiedColumn::from_compound_identifier(parts) else {
            return Ok(());
        };
        if !self.sink.matches_name(column.table.as_str()) || locals.contains(&column.table.key()) {
            return Ok(());
        }
        let key = column.column.key();
        if self
            .ambiguous
            .is_some_and(|ambiguous| ambiguous.contains(&key))
        {
            return Err(RewriteError::unsupported(
                ErrorKind::InvalidSinkColumn,
                format!(
                    "ambiguous _OUTPUT_ column '{}': multiple projected expressions share this output name",
                    column.column.as_str()
                ),
            ));
        }
        if let Some(value) = self.columns.get(&key) {
            // Moving an expression into a subquery must not capture its columns.
            // Ambiguous cases need projection materialization, not guessed names.
            if !locals.is_empty() && captures(value, locals) {
                return Err(RewriteError::unsupported_statement(format!(
                    "policy sink substitution would capture a column in subquery scope: {}.{}",
                    self.sink,
                    column.column.as_str()
                )));
            }
            *expr = value.clone();
        } else if self.ambiguous.is_some() {
            return Err(RewriteError::catalog(
                ErrorKind::InvalidSinkColumn,
                format!(
                    "{}.{} is not a projected output column",
                    self.sink,
                    column.column.as_str()
                ),
                Some(self.sink.to_string()),
                Some(column.column.as_str().to_string()),
            ));
        }
        Ok(())
    }
}

struct ScopedExpressions<'a, 'b> {
    replacement: &'a Replacement<'b>,
    locals: &'a HashSet<String>,
    query_depth: usize,
    replace_expressions: bool,
}
impl VisitorMut for ScopedExpressions<'_, '_> {
    type Break = RewriteError;
    fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
        if self.query_depth == 0
            && let Err(error) = self.replacement.query(query, self.locals)
        {
            return ControlFlow::Break(error);
        }
        self.query_depth += 1;
        ControlFlow::Continue(())
    }
    fn post_visit_query(&mut self, _: &mut Query) -> ControlFlow<Self::Break> {
        self.query_depth -= 1;
        ControlFlow::Continue(())
    }
    fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        if self.replace_expressions
            && self.query_depth == 0
            && let Err(error) = self.replacement.expression(expr, self.locals)
        {
            return ControlFlow::Break(error);
        }
        ControlFlow::Continue(())
    }
}

fn captures(expr: &Expr, locals: &HashSet<String>) -> bool {
    struct Capture<'a>(&'a HashSet<String>);
    impl Visitor for Capture<'_> {
        type Break = ();
        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
            match expr {
                Expr::Identifier(_) => ControlFlow::Break(()),
                Expr::CompoundIdentifier(parts)
                    if QualifiedColumn::from_compound_identifier(parts)
                        .is_some_and(|column| self.0.contains(&column.table.key())) =>
                {
                    ControlFlow::Break(())
                }
                _ => ControlFlow::Continue(()),
            }
        }
    }
    // Conservative for mapped expressions that contain their own subqueries:
    // reject possible capture rather than silently change binding semantics.
    matches!(expr.visit(&mut Capture(locals)), ControlFlow::Break(()))
}
