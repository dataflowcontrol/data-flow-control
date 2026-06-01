//! Single-parse policy constraint compilation shared by catalog validation and registration.

use sqlparser::ast::Expr;

use crate::diagnostics::RewriteError;
use crate::identifiers::QualifiedColumn;
use crate::rewriter::{decompose_composed_aggregates, preprocess_policy_constraint};
use crate::semiring::{SemiringAnalysis, semiring_analysis_from_expr};
use crate::sql::{collect_qualified_columns_from_expr, parse_projection_expr};
use crate::threshold::ThresholdPredicate;

/// Constraint text and AST parsed once for registration-time work.
#[derive(Debug, Clone)]
pub struct ParsedPolicyConstraint {
    pub sql: String,
    pub expr: Expr,
    pub qualified_columns: Vec<QualifiedColumn>,
    pub unqualified_columns: Vec<String>,
    pub semiring: SemiringAnalysis,
    pub(crate) threshold: Option<ThresholdPredicate>,
}

/// Parse and derive registration-time metadata from a policy constraint string.
pub fn parse_policy_constraint(constraint: &str) -> Result<ParsedPolicyConstraint, RewriteError> {
    let sql = preprocess_policy_constraint(constraint);
    let expr = parse_projection_expr(&sql).map_err(|_| {
        RewriteError::unsupported_statement(format!("Invalid constraint SQL expression '{sql}'"))
    })?;
    let expr = decompose_composed_aggregates(expr);
    let qualified_columns = collect_qualified_columns_from_expr(&expr);
    let unqualified_columns = UnqualifiedColumnCollector::collect(&expr);
    let semiring = semiring_analysis_from_expr(&expr);
    let threshold = crate::threshold::threshold_predicate_from_expr(&expr);
    Ok(ParsedPolicyConstraint {
        sql,
        expr,
        qualified_columns,
        unqualified_columns,
        semiring,
        threshold,
    })
}

struct UnqualifiedColumnCollector {
    found: Vec<String>,
}

impl UnqualifiedColumnCollector {
    fn collect(expr: &Expr) -> Vec<String> {
        let mut collector = Self { found: Vec::new() };
        collector.visit(expr, false);
        collector.found
    }

    fn visit(&mut self, expr: &Expr, inside_aggregate: bool) {
        if let Expr::Identifier(ident) = expr {
            let _ = inside_aggregate;
            self.found.push(ident.value.clone());
            return;
        }
        if let Expr::Function(function) = expr {
            self.visit_function(function);
            return;
        }
        match expr {
            Expr::BinaryOp { left, right, .. } => {
                self.visit(left, inside_aggregate);
                self.visit(right, inside_aggregate);
            }
            Expr::Nested(inner)
            | Expr::UnaryOp { expr: inner, .. }
            | Expr::IsNull(inner)
            | Expr::IsNotNull(inner) => self.visit(inner, inside_aggregate),
            _ => {}
        }
    }

    fn visit_function(&mut self, function: &sqlparser::ast::Function) {
        use sqlparser::ast::{FunctionArg, FunctionArgExpr, FunctionArguments};
        let FunctionArguments::List(args) = &function.args else {
            return;
        };
        for arg in &args.args {
            match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(expr),
                    ..
                }
                | FunctionArg::ExprNamed {
                    arg: FunctionArgExpr::Expr(expr),
                    ..
                } => self.visit(expr, true),
                _ => {}
            }
        }
    }
}
