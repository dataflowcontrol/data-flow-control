use std::collections::{HashMap, HashSet};

use smallvec::SmallVec;
use sqlparser::ast::{BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments};

use crate::identifiers::TableKey;
use crate::policy::{PolicyIr, Resolution};
use crate::policy_store::PolicyStore;
use crate::sql::parse_projection_expr;

use super::analysis::policy_source_keys;

pub fn split_select_policies_for_nullable_joins_for_store(
    store: &PolicyStore,
    select: &sqlparser::ast::Select,
    direct_base_tables: &HashSet<TableKey>,
    sink: Option<&TableKey>,
) -> Option<Vec<PolicyIr>> {
    if super::analysis::select_nullable_source_tables(select).is_empty() {
        return None;
    }
    let scope_policies = store.candidate_entries_for_scope(direct_base_tables, sink);
    split_select_policy_entries_for_nullable_joins(
        &scope_policies,
        store,
        select,
        direct_base_tables,
    )
}

pub fn split_select_policies_for_nullable_joins(
    policies: &[PolicyIr],
    select: &sqlparser::ast::Select,
    direct_base_tables: &HashSet<TableKey>,
) -> Option<Vec<PolicyIr>> {
    if super::analysis::select_nullable_source_tables(select).is_empty() {
        return None;
    }
    let mut split_policies = Vec::new();
    let mut changed = false;
    for policy in policies {
        if policy.sources().len() <= 1 {
            split_policies.push(policy.clone());
            continue;
        }
        let Some(split) =
            split_policy_by_source_local_conjuncts(policy, None, None, direct_base_tables)
        else {
            split_policies.push(policy.clone());
            continue;
        };
        changed = true;
        split_policies.extend(split);
    }
    changed.then_some(split_policies)
}

fn split_select_policy_entries_for_nullable_joins(
    policies: &[(usize, PolicyIr)],
    store: &PolicyStore,
    select: &sqlparser::ast::Select,
    direct_base_tables: &HashSet<TableKey>,
) -> Option<Vec<PolicyIr>> {
    let _ = select;
    let mut split_policies = Vec::new();
    let mut changed = false;
    for (index, policy) in policies {
        if policy.sources().len() <= 1 {
            split_policies.push(policy.clone());
            continue;
        }
        let Some(split) = split_policy_by_source_local_conjuncts(
            policy,
            store
                .compiled(*index)
                .and_then(|c| c.source_local_conjuncts.as_ref()),
            store.clone_constraint_ast(*index).as_ref(),
            direct_base_tables,
        ) else {
            split_policies.push(policy.clone());
            continue;
        };
        changed = true;
        split_policies.extend(split);
    }
    changed.then_some(split_policies)
}

pub fn split_policy_by_source_local_conjuncts(
    policy: &PolicyIr,
    cached_conjuncts: Option<&SmallVec<[(TableKey, Expr); 4]>>,
    constraint_ast: Option<&Expr>,
    available_tables: &HashSet<TableKey>,
) -> Option<Vec<PolicyIr>> {
    if let Some(conjuncts) = cached_conjuncts
        && let Some(split) = split_policy_from_cached_conjuncts(policy, conjuncts, available_tables)
    {
        return Some(split);
    }
    split_policy_by_source_local_conjuncts_from_ast(policy, constraint_ast, available_tables)
}

pub(crate) fn compile_source_local_conjuncts(
    constraint: &Expr,
    source_keys: &[TableKey],
) -> Option<SmallVec<[(TableKey, Expr); 4]>> {
    if source_keys.len() <= 1 {
        return None;
    }
    let policy_sources: HashSet<_> = source_keys.iter().cloned().collect();
    let mut grouped: HashMap<TableKey, Vec<Expr>> = HashMap::new();
    for conjunct in split_conjuncts(constraint.clone()) {
        let refs = expr_referenced_policy_sources(&conjunct, &policy_sources);
        if refs.len() != 1 {
            return None;
        }
        let source = refs.into_iter().next()?;
        grouped.entry(source).or_default().push(conjunct);
    }
    if grouped.is_empty() {
        return None;
    }
    let mut result = SmallVec::new();
    for source in source_keys {
        let Some(exprs) = grouped.remove(source) else {
            continue;
        };
        result.push((source.clone(), join_conjuncts(exprs)));
    }
    (!result.is_empty()).then_some(result)
}

pub(crate) fn compile_constraint_referenced_source_keys(
    constraint: &Expr,
    source_keys: &[TableKey],
) -> SmallVec<[TableKey; 4]> {
    let policy_sources: HashSet<_> = source_keys.iter().cloned().collect();
    let mut refs = HashSet::new();
    collect_referenced_policy_sources(constraint, &policy_sources, &mut refs);
    let mut sorted = refs.into_iter().collect::<Vec<_>>();
    sorted.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    sorted.into_iter().collect::<SmallVec<[TableKey; 4]>>()
}

fn split_policy_from_cached_conjuncts(
    policy: &PolicyIr,
    conjuncts: &SmallVec<[(TableKey, Expr); 4]>,
    available_tables: &HashSet<TableKey>,
) -> Option<Vec<PolicyIr>> {
    let PolicyIr::Pgn {
        sources,
        required_sources,
        dimension_tables,
        dimension_aliases,
        dimension_queries,
        sink,
        sink_alias,
        on_fail,
        description,
        ..
    } = policy;
    if sink.is_some() || !required_sources.is_empty() || !dimension_queries.is_empty() {
        return None;
    }
    let _ = dimension_tables;
    let _ = dimension_aliases;
    if !matches!(on_fail, Resolution::Remove | Resolution::Kill) {
        return None;
    }

    let mut split = Vec::new();
    for source in sources {
        let source_key = TableKey::new(source);
        if !available_tables.contains(&source_key) {
            return None;
        }
        let Some((_, expr)) = conjuncts.iter().find(|(key, _)| key == &source_key) else {
            continue;
        };
        split.push(PolicyIr::Pgn {
            sources: vec![source.clone()],
            required_sources: Vec::new(),
            dimension_tables: dimension_tables.clone(),
            dimension_aliases: dimension_aliases.clone(),
            dimension_queries: dimension_queries.clone(),
            sink: None,
            sink_alias: sink_alias.clone(),
            source_aliases: aliases_for_sources(
                policy.source_aliases(),
                std::slice::from_ref(source),
            ),
            constraint: expr.to_string(),
            on_fail: on_fail.clone(),
            description: description.clone(),
        });
    }
    (!split.is_empty()).then_some(split)
}

fn split_policy_by_source_local_conjuncts_from_ast(
    policy: &PolicyIr,
    constraint_ast: Option<&Expr>,
    available_tables: &HashSet<TableKey>,
) -> Option<Vec<PolicyIr>> {
    let PolicyIr::Pgn {
        sources,
        required_sources,
        dimension_tables,
        dimension_aliases,
        dimension_queries,
        sink,
        sink_alias,
        source_aliases,
        constraint,
        on_fail,
        description,
        ..
    } = policy;
    if sink.is_some() || !required_sources.is_empty() || !dimension_queries.is_empty() {
        return None;
    }
    let _ = dimension_tables;
    let _ = dimension_aliases;
    if !matches!(on_fail, Resolution::Remove | Resolution::Kill) {
        return None;
    }

    let policy_sources = policy_source_keys(sources);
    let expr = constraint_ast
        .cloned()
        .or_else(|| parse_constraint_expr(constraint).ok())?;
    let mut constraints_by_source: HashMap<TableKey, Vec<Expr>> = HashMap::new();
    for conjunct in split_conjuncts(expr) {
        let refs = expr_referenced_policy_sources(&conjunct, &policy_sources);
        if refs.len() != 1 {
            return None;
        }
        let source = refs.into_iter().next()?;
        if !available_tables.contains(&source) {
            return None;
        }
        constraints_by_source
            .entry(source)
            .or_default()
            .push(conjunct);
    }

    let mut split = Vec::new();
    for source in sources {
        let source_key = TableKey::new(source);
        let Some(constraints) = constraints_by_source.remove(&source_key) else {
            continue;
        };
        split.push(PolicyIr::Pgn {
            sources: vec![source.clone()],
            required_sources: Vec::new(),
            dimension_tables: dimension_tables.clone(),
            dimension_aliases: dimension_aliases.clone(),
            dimension_queries: dimension_queries.clone(),
            sink: None,
            sink_alias: sink_alias.clone(),
            source_aliases: aliases_for_sources(source_aliases, std::slice::from_ref(source)),
            constraint: join_conjuncts(constraints).to_string(),
            on_fail: on_fail.clone(),
            description: description.clone(),
        });
    }
    (!split.is_empty()).then_some(split)
}

fn aliases_for_sources(
    source_aliases: &HashMap<String, String>,
    sources: &[String],
) -> HashMap<String, String> {
    source_aliases
        .iter()
        .filter(|(_, base)| sources.iter().any(|source| source == *base))
        .map(|(alias, base)| (alias.clone(), base.clone()))
        .collect()
}

pub(crate) fn split_conjuncts(expr: Expr) -> Vec<Expr> {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            let mut conjuncts = split_conjuncts(*left);
            conjuncts.extend(split_conjuncts(*right));
            conjuncts
        }
        Expr::Nested(expr) => split_conjuncts(*expr),
        expr => vec![expr],
    }
}

pub(crate) fn join_conjuncts(mut conjuncts: Vec<Expr>) -> Expr {
    let first = conjuncts.remove(0);
    conjuncts
        .into_iter()
        .fold(first, |left, right| Expr::BinaryOp {
            left: Box::new(left),
            op: BinaryOperator::And,
            right: Box::new(right),
        })
}

pub(crate) fn parse_constraint_expr(sql: &str) -> Result<Expr, String> {
    parse_projection_expr(sql).map_err(|err| err.to_string())
}

pub(crate) fn expr_referenced_policy_sources(
    expr: &Expr,
    policy_sources: &HashSet<TableKey>,
) -> HashSet<TableKey> {
    let mut refs = HashSet::new();
    collect_referenced_policy_sources(expr, policy_sources, &mut refs);
    refs
}

pub(crate) fn collect_referenced_policy_sources(
    expr: &Expr,
    policy_sources: &HashSet<TableKey>,
    refs: &mut HashSet<TableKey>,
) {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            let table_key = TableKey::new(&parts[0].value);
            if policy_sources.contains(&table_key) {
                refs.insert(table_key);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_referenced_policy_sources(left, policy_sources, refs);
            collect_referenced_policy_sources(right, policy_sources, refs);
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr) => {
            collect_referenced_policy_sources(expr, policy_sources, refs);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_referenced_policy_sources(expr, policy_sources, refs);
            collect_referenced_policy_sources(low, policy_sources, refs);
            collect_referenced_policy_sources(high, policy_sources, refs);
        }
        Expr::InList { expr, list, .. } => {
            collect_referenced_policy_sources(expr, policy_sources, refs);
            for expr in list {
                collect_referenced_policy_sources(expr, policy_sources, refs);
            }
        }
        Expr::Function(function) => {
            if let FunctionArguments::List(args) = &function.args {
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
                        } => collect_referenced_policy_sources(expr, policy_sources, refs),
                        _ => {}
                    }
                }
            }
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                collect_referenced_policy_sources(operand, policy_sources, refs);
            }
            for expr in conditions.iter().chain(results.iter()) {
                collect_referenced_policy_sources(expr, policy_sources, refs);
            }
            if let Some(else_result) = else_result {
                collect_referenced_policy_sources(else_result, policy_sources, refs);
            }
        }
        _ => {}
    }
}
