//! Positive provenance only: EXCEPT removes public keys before any policy
//! predicate or resolution is evaluated. Negative contributors are not modeled.
use super::*;
use sqlparser::ast::{SetOperator, Visit, Visitor};

/// Scope analysis, not instrumentation. Reject negative dependencies before
/// ordinary SOURCE-presence skipping or legacy branch planning can hide them.
pub(super) fn validate_dependencies(
    rewriter: &PassantRewriter,
    query: &Query,
    policies: &[usize],
) -> Result<bool, RewriteError> {
    struct Dependencies<'a> {
        rewriter: &'a PassantRewriter,
        policies: &'a [usize],
        scopes: Vec<HashMap<TableKey, HashSet<TableKey>>>,
        has_except: bool,
    }
    impl Dependencies<'_> {
        fn expand(&self, tables: HashSet<TableKey>) -> HashSet<TableKey> {
            let mut result = HashSet::new();
            let mut pending: Vec<_> = tables.into_iter().collect();
            let mut seen = HashSet::new();
            while let Some(table) = pending.pop() {
                if !seen.insert(table.clone()) {
                    continue;
                }
                if let Some(sources) = self.scopes.iter().rev().find_map(|s| s.get(&table)) {
                    // A nonrecursive CTE may shadow its physical input name.
                    // That self-reference denotes the base table, not recursion.
                    if sources.contains(&table) {
                        result.insert(table.clone());
                    }
                    pending.extend(sources.iter().filter(|source| *source != &table).cloned());
                } else {
                    result.insert(table);
                }
            }
            result
        }
        fn check(&mut self, body: &SetExpr) -> Result<(), RewriteError> {
            if let SetExpr::SetOperation {
                op, left, right, ..
            } = body
            {
                if matches!(op, SetOperator::Except) {
                    self.has_except = true;
                    let negative = self.expand(crate::source_sets::set_expr_source_tables(right));
                    for index in self.policies {
                        let policy = self.rewriter.store.policy(*index).unwrap();
                        for source in policy.sources() {
                            if negative.contains(&TableKey::new(source)) {
                                return Err(RewriteError::unsupported_statement(format!(
                                    "policy {index} ({}) requires negative-side SOURCE {source} provenance across EXCEPT; negation provenance is not supported",
                                    policy.constraint()
                                )));
                            }
                        }
                    }
                }
                self.check(left)?;
                self.check(right)?;
            }
            Ok(())
        }
    }
    impl Visitor for Dependencies<'_> {
        type Break = RewriteError;
        fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
            let mut scope = HashMap::new();
            if let Some(with) = &query.with {
                for cte in &with.cte_tables {
                    scope.insert(
                        TableKey::new(&cte.alias.name.value),
                        crate::source_sets::set_expr_source_tables(&cte.query.body),
                    );
                }
            }
            self.scopes.push(scope);
            match self.check(&query.body) {
                Ok(()) => ControlFlow::Continue(()),
                Err(error) => ControlFlow::Break(error),
            }
        }
        fn post_visit_query(&mut self, _: &Query) -> ControlFlow<Self::Break> {
            self.scopes.pop();
            ControlFlow::Continue(())
        }
    }
    let mut visitor = Dependencies {
        rewriter,
        policies,
        scopes: Vec::new(),
        has_except: false,
    };
    if let ControlFlow::Break(error) = query.visit(&mut visitor) {
        return Err(error);
    }
    Ok(visitor.has_except)
}

pub(super) fn finalize(
    rewriter: &PassantRewriter,
    left: &SetExpr,
    right: &SetExpr,
    left_schema: &Synthesized,
    right_schema: &Synthesized,
    context: &RewriteContext,
    frame: &JointLineageContext,
) -> Result<(SetExpr, Synthesized), RewriteError> {
    if !left_schema.finalized.is_empty() || !right_schema.finalized.is_empty() {
        return Err(unsupported("EXCEPT DISTINCT over finalized policy state"));
    }
    let side_name = Ident::new(fresh(&mut frame.state.borrow_mut().used));
    let side_key = TableKey::new(&side_name.value);
    let left_alias = Ident::new(fresh(&mut frame.state.borrow_mut().used));
    let marked_left = parsed_body(&format!(
        "SELECT {left_alias}.*, TRUE AS {side_name} FROM ({left}) AS {left_alias}"
    ))?;
    let mut left_outputs = left_schema.outputs.clone();
    left_outputs.push(OutputColumn {
        name: side_name,
        origin: None,
        public: false,
        presence: Some(side_key.clone()),
    });
    // The negative input supplies only exclusion keys, not positive provenance.
    let right_outputs: Vec<_> = right_schema
        .outputs
        .iter()
        .filter(|c| c.public)
        .map(|c| OutputColumn {
            name: c.name.clone(),
            origin: None,
            public: true,
            presence: None,
        })
        .collect();
    let mut raw_frame = frame.clone();
    raw_frame.root = false;
    let (aligned, mut schema) = align_union_all(
        rewriter,
        &marked_left,
        right,
        &left_outputs,
        &right_outputs,
        &raw_frame,
    )?;
    let tag = schema
        .outputs
        .iter()
        .find(|c| c.presence.as_ref() == Some(&side_key))
        .unwrap()
        .name
        .clone();
    schema
        .outputs
        .retain(|c| c.presence.as_ref() != Some(&side_key));
    let cte = Ident::new(fresh(&mut frame.state.borrow_mut().used));
    let lhs = Ident::new(fresh(&mut frame.state.borrow_mut().used));
    let rhs = Ident::new(fresh(&mut frame.state.borrow_mut().used));
    let keys = schema
        .outputs
        .iter()
        .filter(|c| c.public)
        .map(|c| format!("{lhs}.{} IS NOT DISTINCT FROM {rhs}.{}", c.name, c.name))
        .collect::<Vec<_>>()
        .join(" AND ");
    let projection = schema
        .outputs
        .iter()
        .map(|c| format!("{lhs}.{} AS {}", c.name, c.name))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "WITH {cte} AS ({aligned}) SELECT {projection} FROM {cte} AS {lhs} WHERE {lhs}.{tag} IS TRUE AND NOT EXISTS (SELECT 1 FROM {cte} AS {rhs} WHERE {rhs}.{tag} IS FALSE AND {keys})"
    );
    let statements =
        sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::DuckDbDialect {}, &sql)
            .map_err(|e| unsupported(&format!("EXCEPT positive contributor SQL: {e}")))?;
    let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
        unreachable!()
    };
    schema.inputs.clear();
    schema.presence.clear();
    terminal_distinct::finalize(rewriter, SetExpr::Query(query), schema, context, frame)
}
