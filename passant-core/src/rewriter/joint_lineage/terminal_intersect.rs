//! Bounded terminal INTERSECT DISTINCT: common-type public keys and idempotent
//! source-local metrics. Additive, bag, shared-origin and nested-state cases are
//! deliberately not interpreted by this lowering.
use super::*;

pub(super) fn validate_policies(
    rewriter: &PassantRewriter,
    policies: &[usize],
) -> Result<(), RewriteError> {
    for index in policies {
        let expr = crate::sql::parse_policy_expr_duckdb(
            rewriter.store.policy(*index).unwrap().constraint(),
        )?;
        let result = visit_expressions(&expr, |expr| {
            if let Expr::Function(f) = expr {
                if !rewriter.aggregate_registry.is_aggregate_call(f) {
                    return ControlFlow::Break(unsupported(
                        "INTERSECT DISTINCT policy scalar function purity is not established",
                    ));
                }
                if rewriter.aggregate_registry.is_aggregate_call(f)
                    && !matches!(
                        f.name.to_string().to_ascii_lowercase().as_str(),
                        "min" | "max"
                    )
                {
                    return ControlFlow::Break(unsupported(
                        "INTERSECT DISTINCT currently requires MIN/MAX or scalar uniqueness, not additive/general aggregate state",
                    ));
                }
            }
            ControlFlow::Continue(())
        });
        if let ControlFlow::Break(error) = result {
            return Err(error);
        }
    }
    Ok(())
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
    let source_set = |schema: &Synthesized| -> HashSet<TableKey> {
        schema
            .outputs
            .iter()
            .filter_map(|c| {
                c.origin
                    .as_ref()
                    .map(|(s, _)| s.clone())
                    .or_else(|| c.presence.clone())
            })
            .collect()
    };
    let left_sources = source_set(left_schema);
    let right_sources = source_set(right_schema);
    if !left_sources.is_disjoint(&right_sources) {
        return Err(unsupported(
            "INTERSECT DISTINCT with shared source origins requires a provenance merge contract",
        ));
    }
    if !left_schema.finalized.is_empty() || !right_schema.finalized.is_empty() {
        return Err(unsupported(
            "INTERSECT DISTINCT over finalized predicate state",
        ));
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
    let mut raw_frame = frame.clone();
    raw_frame.root = false;
    // UNION ALL first establishes exactly the set operation's common public
    // types. Pairing raw heterogeneous arm types directly would be unsound.
    let (aligned, mut schema) = align_union_all(
        rewriter,
        &marked_left,
        right,
        &left_outputs,
        &right_schema.outputs,
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
    let cte_name = Ident::new(fresh(&mut frame.state.borrow_mut().used));
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
        .map(|c| {
            let source = c.origin.as_ref().map(|(s, _)| s).or(c.presence.as_ref());
            let alias = if source.is_some_and(|s| right_sources.contains(s)) {
                &rhs
            } else {
                &lhs
            };
            format!("{alias}.{} AS {}", c.name, c.name)
        })
        .collect::<Vec<_>>()
        .join(", ");
    // The CTE belongs to the raw contributor query, not to a generated Select
    // traversal. Keep its aliases visible to the projection before finalization.
    let sql = format!(
        "WITH {cte_name} AS ({aligned}) SELECT {projection} FROM {cte_name} AS {lhs} JOIN {cte_name} AS {rhs} ON {keys} WHERE {lhs}.{tag} IS TRUE AND {rhs}.{tag} IS FALSE"
    );
    let statements =
        sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::DuckDbDialect {}, &sql)
            .map_err(|e| unsupported(&format!("INTERSECT contributor SQL: {e}")))?;
    let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
        unreachable!()
    };
    let paired = SetExpr::Query(query);
    schema.inputs.clear();
    schema.presence.clear();
    terminal_distinct::finalize(rewriter, paired, schema, context, frame)
}
