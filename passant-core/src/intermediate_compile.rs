//! Lower registered policy ASTs to exportable whole-input aggregate demands.
//!
//! Normal registration remains the sole parser/normalizer (including UNIQUE and
//! AVG decomposition). This is a state-target lowering of `CompiledPolicy`, not
//! a second policy language or SQLGlot compiler.
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use serde::Serialize;
use sqlparser::ast::{
    BinaryOperator, DuplicateTreatment, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, Ident, UnaryOperator, Value, Visit, VisitMut, Visitor, VisitorMut,
    visit_expressions, visit_expressions_mut,
};

use crate::aggregate_registry::{AggregateRegistry, function_name};
use crate::diagnostics::RewriteError;
use crate::intermediate_state::IntermediateSpec;
use crate::policy_store::CompiledPolicy;
use crate::rewriter::PassantRewriter;

#[derive(Debug, Clone, Serialize)]
pub struct IntermediateCompilation {
    pub specs: Vec<IntermediateSpec>,
    pub predicate: String,
    pub sink_columns: Vec<(String, String)>,
    pub sources: Vec<String>,
    pub required_sources: Vec<String>,
}

fn error(message: impl Into<String>) -> RewriteError {
    RewriteError::unsupported_statement(message.into())
}
fn column(name: &str) -> Expr {
    Expr::Identifier(Ident::with_quote('"', name))
}
fn guarded(mut expression: Expr, guards: Vec<Expr>) -> Expr {
    for guard in guards {
        expression = Expr::BinaryOp {
            left: Box::new(guard),
            op: BinaryOperator::And,
            right: Box::new(Expr::Nested(Box::new(expression))),
        };
    }
    expression
}

impl CompiledPolicy {
    pub fn intermediate_compilation(
        &self,
        registry: &AggregateRegistry,
    ) -> Result<IntermediateCompilation, RewriteError> {
        if !self.policy.dimension_tables().is_empty() || !self.policy.dimension_queries().is_empty()
        {
            return Err(error("Intermediate compilation does not support DIMENSION"));
        }
        let ast = &self
            .constraint
            .as_ref()
            .ok_or_else(|| error("Missing registered constraint AST"))?
            .ast;
        let mut sources = HashMap::new();
        for source in self.policy.sources() {
            sources.insert(source.to_ascii_lowercase(), source.clone());
        }
        let mut aliases = sources.clone();
        for (alias, source) in self.policy.source_aliases() {
            aliases.insert(alias.to_ascii_lowercase(), source.clone());
        }
        let mut sink_names = HashSet::from(["_output_".to_string()]);
        if let Some(name) = self.policy.sink() {
            sink_names.insert(name.to_ascii_lowercase());
        }
        if let Some(name) = self.policy.sink_alias() {
            sink_names.insert(name.to_ascii_lowercase());
        }
        let mut compiler = Compiler {
            registry,
            sources,
            aliases,
            sink_names,
            specs: Vec::new(),
            sink_columns: Vec::new(),
        };
        compiler.validate(ast)?;
        let (predicate, guards) = compiler.lower(ast)?;
        Ok(IntermediateCompilation {
            specs: compiler.specs,
            predicate: guarded(predicate, guards).to_string(),
            sink_columns: compiler.sink_columns,
            sources: self.policy.sources().to_vec(),
            required_sources: self.policy.required_sources().to_vec(),
        })
    }
}

impl PassantRewriter {
    /// Uses the existing registration-time AST and stable original policy ID.
    pub fn compile_intermediate_policy(
        &self,
        policy_index: usize,
    ) -> Result<IntermediateCompilation, RewriteError> {
        let compiled = self
            .store
            .policy_indices()
            .into_iter()
            .filter_map(|index| self.store.compiled(index))
            .find(|policy| policy.original_policy_index == policy_index)
            .ok_or_else(|| error("Unknown active policy registration ID"))?;
        compiled.intermediate_compilation(&self.aggregate_registry)
    }
}

struct Compiler<'a> {
    registry: &'a AggregateRegistry,
    sources: HashMap<String, String>,
    aliases: HashMap<String, String>,
    sink_names: HashSet<String>,
    specs: Vec<IntermediateSpec>,
    sink_columns: Vec<(String, String)>,
}

impl Compiler<'_> {
    fn source_of(&self, expr: &Expr) -> Result<Option<String>, RewriteError> {
        let Expr::CompoundIdentifier(parts) = expr else {
            return Err(error(format!("Unbound intermediate policy column: {expr}")));
        };
        let qualifier = parts[..parts.len() - 1]
            .iter()
            .map(|p| p.value.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(".");
        if let Some(source) = self.aliases.get(&qualifier) {
            return Ok(Some(source.clone()));
        }
        if self.sink_names.contains(&qualifier) {
            return Ok(None);
        }
        Err(error(format!("Unbound intermediate policy column: {expr}")))
    }

    fn columns(&self, expr: &Expr) -> Result<Vec<Expr>, RewriteError> {
        let mut columns = Vec::new();
        let result = visit_expressions(expr, |node| {
            if matches!(node, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
                if let Err(error) = self.source_of(node) {
                    return ControlFlow::Break(error);
                }
                columns.push(node.clone());
            }
            ControlFlow::Continue(())
        });
        if let ControlFlow::Break(error) = result {
            return Err(error);
        }
        Ok(columns)
    }

    fn validate(&self, expr: &Expr) -> Result<(), RewriteError> {
        let result = visit_expressions(expr, |node| {
            match node {
                Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
                    return ControlFlow::Break(error(
                        "Subqueries are unsupported in intermediate policies",
                    ));
                }
                Expr::Function(f) => {
                    if f.over.is_some()
                        || f.filter.is_some()
                        || !f.within_group.is_empty()
                        || matches!(f.args, FunctionArguments::Subquery(_))
                        || matches!(f.parameters, FunctionArguments::Subquery(_))
                    {
                        return ControlFlow::Break(error(
                            "Subqueries, windows and filtered aggregates are unsupported",
                        ));
                    }
                    let name = function_name(f).to_ascii_lowercase();
                    if !self.registry.is_aggregate_call(f) && !known_scalar(&name) {
                        return ControlFlow::Break(error(format!(
                            "Unclassified policy function: {name}"
                        )));
                    }
                }
                _ => (),
            }
            ControlFlow::Continue(())
        });
        if let ControlFlow::Break(error) = result {
            return Err(error);
        }
        Ok(())
    }

    fn demand(&mut self, expr: &Expr, aggregate: &str) -> Result<Expr, RewriteError> {
        let columns = self.columns(expr)?;
        let mut involved = columns
            .iter()
            .map(|column| self.source_of(column))
            .collect::<Result<HashSet<_>, _>>()?;
        if columns.is_empty() && self.sources.len() == 1 {
            involved.insert(self.sources.values().next().cloned());
        }
        if involved.len() != 1 || involved.contains(&None) {
            return Err(error(
                "An intermediate expression must belong to exactly one SOURCE",
            ));
        }
        let source = involved.into_iter().next().flatten().unwrap();
        let mut local = expr.clone();
        let _ = visit_expressions_mut(&mut local, |node| {
            if let Expr::CompoundIdentifier(parts) = node {
                *node = column(&parts.last().unwrap().value);
            }
            ControlFlow::<()>::Continue(())
        });
        let name = format!("_metric_{}", self.specs.len());
        self.specs.push(IntermediateSpec {
            name: name.clone(),
            source,
            expression: local.to_string(),
            aggregate: aggregate.into(),
        });
        Ok(column(&name))
    }

    fn aggregate_argument(function: &Function) -> Result<(Expr, bool), RewriteError> {
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

    fn lower(&mut self, node: &Expr) -> Result<(Expr, Vec<Expr>), RewriteError> {
        // UNIQUE is normalized by the core parser into COUNT(DISTINCT) =/!= 1.
        if let Expr::BinaryOp {
            left,
            op: op @ (BinaryOperator::Eq | BinaryOperator::NotEq),
            right,
        } = node
            && let Expr::Function(function) = left.as_ref()
            && function_name(function).eq_ignore_ascii_case("count")
            && let (argument, true) = Self::aggregate_argument(function)?
            && matches!(right.as_ref(), Expr::Value(Value::Number(n, _)) if n == "1")
        {
            let metric = self.demand(&argument, "unique")?;
            let Expr::Identifier(name) = metric else {
                unreachable!()
            };
            return Ok((
                Expr::BinaryOp {
                    left: Box::new(column(&format!("{}__cardinality", name.value))),
                    op: op.clone(),
                    right: right.clone(),
                },
                Vec::new(),
            ));
        }
        if let Expr::Function(function) = node
            && self.registry.is_aggregate_call(function)
        {
            let name = function_name(function).to_ascii_lowercase();
            if !matches!(
                name.as_str(),
                "min" | "max" | "sum" | "count" | "avg" | "bool_and" | "bool_or"
            ) {
                return Err(error(format!("Unsupported policy aggregate: {name}")));
            }
            let (argument, distinct) = Self::aggregate_argument(function)?;
            if distinct {
                return Err(error("Only COUNT(DISTINCT expression) =/!= 1 is compact"));
            }
            if self.registry.expr_contains_aggregate(&argument) {
                return Err(error("Nested policy aggregates are unsupported"));
            }
            return Ok((self.demand(&argument, &name)?, Vec::new()));
        }
        if matches!(
            node,
            Expr::UnaryOp {
                op: UnaryOperator::Not,
                ..
            }
        ) {
            let mut check = SourceOutsideAggregate {
                compiler: self,
                aggregate: Vec::new(),
            };
            if let ControlFlow::Break(error) = node.visit(&mut check) {
                return Err(error);
            }
        }
        let columns = self.columns(node)?;
        let all_source = !columns.is_empty()
            && columns
                .iter()
                .all(|column| self.source_of(column).is_ok_and(|source| source.is_some()));
        if all_source && !is_boundary(node) && !self.registry.expr_contains_aggregate(node) {
            let metric = self.demand(node, "unique")?;
            let Expr::Identifier(name) = &metric else {
                unreachable!()
            };
            return Ok((
                metric.clone(),
                vec![Expr::BinaryOp {
                    left: Box::new(column(&format!("{}__cardinality", name.value))),
                    op: BinaryOperator::Eq,
                    right: Box::new(Expr::Value(Value::Number("1".into(), false))),
                }],
            ));
        }
        if let Expr::CompoundIdentifier(parts) = node {
            self.source_of(node)?;
            let name = format!("_sink_{}", self.sink_columns.len());
            self.sink_columns
                .push((name.clone(), parts.last().unwrap().value.clone()));
            return Ok((column(&name), Vec::new()));
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
            compiler: self,
            depth: 0,
            boolean,
            guards: Vec::new(),
        };
        if let ControlFlow::Break(error) = VisitMut::visit(&mut transformed, &mut children) {
            return Err(error);
        }
        let guards = children.guards;
        if is_comparison(node) {
            Ok((guarded(transformed, guards), Vec::new()))
        } else {
            Ok((transformed, guards))
        }
    }
}

// Visit only immediate child expressions. Each child is lowered recursively by
// the compiler; generated metric identifiers are never visited as user SQL.
struct Children<'a, 'b> {
    compiler: &'a mut Compiler<'b>,
    depth: usize,
    boolean: bool,
    guards: Vec<Expr>,
}
impl VisitorMut for Children<'_, '_> {
    type Break = RewriteError;
    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        if self.depth == 1 {
            match self.compiler.lower(expr) {
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
struct SourceOutsideAggregate<'a, 'b> {
    compiler: &'a Compiler<'b>,
    aggregate: Vec<bool>,
}
impl Visitor for SourceOutsideAggregate<'_, '_> {
    type Break = RewriteError;
    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        let inside = self.aggregate.last().copied().unwrap_or(false)
            || matches!(expr, Expr::Function(f) if self.compiler.registry.is_aggregate_call(f));
        if !inside && matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
            match self.compiler.source_of(expr) {
                Ok(Some(_)) => {
                    return ControlFlow::Break(error(
                        "Negated implicit source uniqueness is unsupported; use explicit aggregates",
                    ));
                }
                Err(error) => return ControlFlow::Break(error),
                _ => (),
            }
        }
        self.aggregate.push(inside);
        ControlFlow::Continue(())
    }
    fn post_visit_expr(&mut self, _: &Expr) -> ControlFlow<Self::Break> {
        self.aggregate.pop();
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
fn is_boundary(expr: &Expr) -> bool {
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
fn known_scalar(name: &str) -> bool {
    matches!(
        name,
        "abs"
            | "lower"
            | "upper"
            | "coalesce"
            | "nullif"
            | "round"
            | "ceil"
            | "ceiling"
            | "floor"
            | "length"
            | "len"
            | "substring"
            | "substr"
            | "trim"
            | "ltrim"
            | "rtrim"
            | "concat"
            | "concat_ws"
            | "replace"
            | "greatest"
            | "least"
            | "if"
            | "ifnull"
            | "power"
            | "pow"
            | "sqrt"
            | "sign"
            | "starts_with"
            | "ends_with"
            | "contains"
            | "regexp_matches"
            | "regexp_replace"
    )
}

/// Bind supplied public sink columns to generated predicate columns in Rust.
pub fn bind_intermediate_sink(
    sink_columns: &[(String, String)],
    sink: &std::collections::BTreeMap<String, crate::intermediate_state::SqlValue>,
) -> Result<std::collections::BTreeMap<String, crate::intermediate_state::SqlValue>, String> {
    let mut bound = std::collections::BTreeMap::new();
    for (generated, original) in sink_columns {
        let value = sink
            .get(original)
            .ok_or_else(|| format!("Missing sink columns: {original}"))?;
        bound.insert(generated.clone(), value.clone());
    }
    Ok(bound)
}
