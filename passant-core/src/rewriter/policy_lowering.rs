//! Shared policy expression analysis and target-dependent lowering.
//! Targets choose how aggregate/source leaves are evaluated; this walker owns
//! recursive traversal and guard placement, so guards stay inside boolean arms.
use crate::aggregate_registry::AggregateRegistry;
use crate::diagnostics::RewriteError;
use crate::sql::render_expr;
use sqlparser::ast::{
    BinaryOperator, DuplicateTreatment, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, UnaryOperator, Value, VisitMut, VisitorMut, visit_expressions,
};
use std::collections::HashSet;
use std::ops::ControlFlow;

pub(crate) type LoweredPolicyExpr = (Expr, Vec<Expr>);
pub(crate) trait PolicyExpressionTarget {
    /// Return None to descend; emitted expressions are never revisited.
    fn lower_atom(&mut self, node: &Expr) -> Result<Option<LoweredPolicyExpr>, RewriteError>;
}
pub(crate) fn lower_policy_expression<T: PolicyExpressionTarget>(
    node: &Expr,
    target: &mut T,
) -> Result<LoweredPolicyExpr, RewriteError> {
    if let Some(lowered) = target.lower_atom(node)? {
        return Ok(lowered);
    }
    let boolean = matches!(
        node,
        Expr::BinaryOp {
            op: BinaryOperator::And | BinaryOperator::Or,
            ..
        } | Expr::UnaryOp {
            op: UnaryOperator::Not,
            ..
        }
    );
    let mut transformed = node.clone();
    let mut children = Children {
        target,
        depth: 0,
        boolean,
        guards: Vec::new(),
    };
    if let ControlFlow::Break(error) = VisitMut::visit(&mut transformed, &mut children) {
        return Err(error);
    }
    let guards = children.guards;
    // Independently lowered arms may now contain OR; sqlparser Display does not
    // restore precedence parentheses. Use the existing SQL builder in both targets.
    if let Expr::BinaryOp {
        left,
        op: BinaryOperator::And,
        right,
    } = transformed
    {
        transformed = crate::sql::builders::and_expr(*left, *right);
    }
    if is_comparison(node) {
        Ok((guarded(transformed, guards), Vec::new()))
    } else {
        Ok((transformed, guards))
    }
}
pub(crate) fn guarded(mut expression: Expr, guards: Vec<Expr>) -> Expr {
    for guard in guards {
        expression = Expr::BinaryOp {
            left: Box::new(guard),
            op: BinaryOperator::And,
            right: Box::new(Expr::Nested(Box::new(expression))),
        };
    }
    expression
}

// Visit only immediate child expressions. Each child is lowered recursively by
// the compiler; generated metric identifiers are never visited as user SQL.
struct Children<'a, T> {
    target: &'a mut T,
    depth: usize,
    boolean: bool,
    guards: Vec<Expr>,
}
impl<T: PolicyExpressionTarget> VisitorMut for Children<'_, T> {
    type Break = RewriteError;
    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        if self.depth == 1 {
            match lower_policy_expression(expr, self.target) {
                Ok((lowered, guards)) => {
                    *expr = if self.boolean {
                        guarded(lowered, guards)
                    } else {
                        self.guards.extend(guards);
                        lowered
                    };
                }
                Err(error) => return ControlFlow::Break(error),
            }
        }
        self.depth += 1;
        ControlFlow::Continue(())
    }
    fn post_visit_expr(&mut self, _: &mut Expr) -> ControlFlow<Self::Break> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }
}
fn is_comparison(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::BinaryOp {
            op: BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Gt
                | BinaryOperator::GtEq
                | BinaryOperator::Lt
                | BinaryOperator::LtEq,
            ..
        } | Expr::Like { .. }
            | Expr::ILike { .. }
            | Expr::Between { .. }
            | Expr::InList { .. }
            | Expr::IsNull(_)
            | Expr::IsNotNull(_)
            | Expr::IsTrue(_)
            | Expr::IsFalse(_)
            | Expr::IsNotTrue(_)
            | Expr::IsNotFalse(_)
    )
}
pub(crate) fn is_boundary(expr: &Expr) -> bool {
    is_comparison(expr)
        || matches!(
            expr,
            Expr::Nested(_)
                | Expr::BinaryOp {
                    op: BinaryOperator::And | BinaryOperator::Or,
                    ..
                }
                | Expr::UnaryOp {
                    op: UnaryOperator::Not,
                    ..
                }
        )
}
pub(crate) fn aggregate_argument(function: &Function) -> Result<(Expr, bool), RewriteError> {
    let FunctionArguments::List(args) = &function.args else {
        return Err(error("Unsupported policy aggregate arguments"));
    };
    if args.args.len() != 1
        || !args.clauses.is_empty()
        || !matches!(function.parameters, FunctionArguments::None)
    {
        return Err(error("Unsupported policy aggregate arguments"));
    }
    let argument = match &args.args[0] {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => expr.clone(),
        FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => {
            Expr::Wildcard(sqlparser::ast::helpers::attached_token::AttachedToken::empty())
        }
        _ => return Err(error("Unsupported policy aggregate argument")),
    };
    Ok((
        argument,
        args.duplicate_treatment == Some(DuplicateTreatment::Distinct),
    ))
}

fn error(message: &str) -> RewriteError {
    RewriteError::unsupported_statement(message)
}

/// Registry classification is shared by live projections and state demands.
pub(crate) fn aggregate_call<'a>(
    expr: &'a Expr,
    registry: &AggregateRegistry,
) -> Option<&'a Function> {
    match expr {
        Expr::Function(f) if registry.is_aggregate_call(f) => Some(f),
        _ => None,
    }
}
pub(crate) fn aggregate_demands(expr: &Expr, registry: &AggregateRegistry) -> Vec<Expr> {
    let mut seen = HashSet::new();
    let mut demands = Vec::new();
    let _: ControlFlow<()> = visit_expressions(expr, |node| {
        if aggregate_call(node, registry).is_some() && seen.insert(render_expr(node, None)) {
            demands.push(node.clone());
        }
        ControlFlow::Continue(())
    });
    demands
}
pub(crate) fn count_distinct_cardinality_one(expr: &Expr) -> Option<(&Function, &BinaryOperator)> {
    if let Expr::Nested(inner) = expr {
        return count_distinct_cardinality_one(inner);
    }
    let Expr::BinaryOp { left, op, right } = expr else {
        return None;
    };
    if !matches!(
        op,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Gt
            | BinaryOperator::Lt
            | BinaryOperator::GtEq
            | BinaryOperator::LtEq
    ) {
        return None;
    }
    let Expr::Function(f) = left.as_ref() else {
        return None;
    };
    if !f.name.to_string().eq_ignore_ascii_case("count") {
        return None;
    }
    if !matches!(&f.args, FunctionArguments::List(a) if a.duplicate_treatment == Some(DuplicateTreatment::Distinct))
    {
        return None;
    }
    if !matches!(right.as_ref(), Expr::Value(Value::Number(n, _)) if n == "1") {
        return None;
    }
    Some((f, op))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewriter::expr::parse_expr;

    struct GuardedColumns;
    impl PolicyExpressionTarget for GuardedColumns {
        fn lower_atom(&mut self, node: &Expr) -> Result<Option<LoweredPolicyExpr>, RewriteError> {
            if let Expr::Identifier(id) = node
                && id.value == "x"
            {
                return Ok(Some((
                    parse_expr("metric").unwrap(),
                    vec![parse_expr("cardinality = 1").unwrap()],
                )));
            }
            Ok(None)
        }
    }
    #[test]
    fn comparison_guards_stay_inside_disjunction_and_negation() {
        for (input, expected) in [
            (
                "x = 3 OR permitted",
                "cardinality = 1 AND (metric = 3) OR permitted",
            ),
            ("NOT (x = 3)", "NOT (cardinality = 1 AND (metric = 3))"),
        ] {
            let (expr, guards) =
                lower_policy_expression(&parse_expr(input).unwrap(), &mut GuardedColumns).unwrap();
            assert!(guards.is_empty());
            assert_eq!(expr.to_string(), expected);
        }
    }
    #[test]
    fn generated_disjunction_is_grouped_in_conjunction() {
        struct LiveLeaf;
        impl PolicyExpressionTarget for LiveLeaf {
            fn lower_atom(
                &mut self,
                node: &Expr,
            ) -> Result<Option<LoweredPolicyExpr>, RewriteError> {
                if matches!(node, Expr::Identifier(_)) {
                    Ok(Some((parse_expr("a OR b").unwrap(), vec![])))
                } else {
                    Ok(None)
                }
            }
        }
        let (expr, _) =
            lower_policy_expression(&parse_expr("x AND y").unwrap(), &mut LiveLeaf).unwrap();
        assert_eq!(expr.to_string(), "(a OR b) AND (a OR b)");
    }
    #[test]
    fn registry_demands_are_deduplicated_in_first_use_order() {
        let registry = AggregateRegistry::for_dialect(crate::SqlDialect::DuckDb);
        let expr = parse_expr("SUM(t.x) + MAX(t.y) + SUM(t.x) + ABS(t.z)").unwrap();
        let demands = aggregate_demands(&expr, &registry);
        assert_eq!(
            demands.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["SUM(t.x)", "MAX(t.y)"]
        );
        assert!(
            count_distinct_cardinality_one(&parse_expr("(COUNT(DISTINCT t.x) != 1)").unwrap())
                .is_some()
        );
        assert!(count_distinct_cardinality_one(&parse_expr("COUNT(t.x) = 1").unwrap()).is_none());
    }
}
