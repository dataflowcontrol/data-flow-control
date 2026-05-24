//! Source-set analysis and policy splitting for outer joins, set operations, and
//! other queries where cross-source policies need branch-local or row-level handling.

use std::collections::{HashMap, HashSet};

use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, JoinOperator, Select,
    SetExpr, TableFactor, TableWithJoins,
};

use crate::identifiers::{TableKey, TableName};
use crate::policy::{PolicyIr, Resolution};
use crate::sql::parse_projection_expr;

pub fn set_expr_source_tables(set_expr: &SetExpr) -> HashSet<TableKey> {
    match set_expr {
        SetExpr::Select(select) => select_source_tables(select),
        SetExpr::Query(query) => set_expr_source_tables(query.body.as_ref()),
        SetExpr::SetOperation { left, right, .. } => {
            let mut tables = set_expr_source_tables(left);
            tables.extend(set_expr_source_tables(right));
            tables
        }
        _ => HashSet::new(),
    }
}

pub fn select_source_tables(select: &Select) -> HashSet<TableKey> {
    let mut tables = HashSet::new();
    for table in &select.from {
        tables.extend(table_with_joins_source_tables(table));
    }
    tables
}

pub fn select_nullable_source_tables(select: &Select) -> HashSet<TableKey> {
    let mut nullable = HashSet::new();
    for table in &select.from {
        let mut left_tables = table_factor_source_tables(&table.relation);
        for join in &table.joins {
            let right_tables = table_factor_source_tables(&join.relation);
            match join.join_operator {
                JoinOperator::LeftOuter(_) => nullable.extend(right_tables.iter().cloned()),
                JoinOperator::RightOuter(_) => nullable.extend(left_tables.iter().cloned()),
                JoinOperator::FullOuter(_) => {
                    nullable.extend(left_tables.iter().cloned());
                    nullable.extend(right_tables.iter().cloned());
                }
                _ => {}
            }
            left_tables.extend(right_tables);
        }
    }
    nullable
}

pub fn select_has_full_join(select: &Select) -> bool {
    select.from.iter().any(|table| {
        table
            .joins
            .iter()
            .any(|join| matches!(join.join_operator, JoinOperator::FullOuter(_)))
    })
}

pub fn select_has_anti_join(select: &Select) -> bool {
    select.from.iter().any(|table| {
        table.joins.iter().any(|join| {
            matches!(
                join.join_operator,
                JoinOperator::Anti(_) | JoinOperator::LeftAnti(_) | JoinOperator::RightAnti(_)
            )
        })
    })
}

pub fn set_operation_requires_cross_source_policies(
    policies: &[PolicyIr],
    left: &SetExpr,
    right: &SetExpr,
) -> bool {
    let left_tables = set_expr_source_tables(left);
    let right_tables = set_expr_source_tables(right);
    if left_tables.is_empty() || right_tables.is_empty() {
        return false;
    }
    let all_tables = left_tables
        .union(&right_tables)
        .cloned()
        .collect::<HashSet<_>>();

    policies.iter().any(|policy| {
        let sources = policy_source_keys(policy.sources());
        sources.len() > 1
            && sources.iter().all(|source| all_tables.contains(source))
            && (!sources.iter().all(|source| left_tables.contains(source))
                || !sources.iter().all(|source| right_tables.contains(source)))
    })
}

pub fn cross_source_policies_for_branch(
    policies: &[PolicyIr],
    branch_tables: &HashSet<TableKey>,
) -> Vec<PolicyIr> {
    policies
        .iter()
        .filter(|policy| {
            policy.sources().len() > 1
                && policy
                    .sources()
                    .iter()
                    .any(|source| branch_tables.contains(&TableKey::new(source)))
        })
        .cloned()
        .collect()
}

pub fn split_select_policies_for_nullable_joins(
    policies: &[PolicyIr],
    select: &Select,
    direct_base_tables: &HashSet<TableKey>,
) -> Option<Vec<PolicyIr>> {
    if select_nullable_source_tables(select).is_empty() {
        return None;
    }
    let mut split_policies = Vec::new();
    let mut changed = false;
    for policy in policies {
        if policy.sources().len() <= 1 {
            split_policies.push(policy.clone());
            continue;
        }
        let Some(split) = split_policy_by_source_local_conjuncts(policy, direct_base_tables) else {
            split_policies.push(policy.clone());
            continue;
        };
        changed = true;
        split_policies.extend(split);
    }
    changed.then_some(split_policies)
}

pub fn split_set_operation_policies(
    policies: &[PolicyIr],
    left: &SetExpr,
    right: &SetExpr,
) -> Option<(Vec<PolicyIr>, Vec<PolicyIr>)> {
    let left_tables = set_expr_source_tables(left);
    let right_tables = set_expr_source_tables(right);
    let mut left_policies = Vec::new();
    let mut right_policies = Vec::new();

    for policy in policies {
        if !policy_requires_set_split(policy, &left_tables, &right_tables) {
            left_policies.push(policy.clone());
            right_policies.push(policy.clone());
            continue;
        }
        let (left_split, right_split) =
            split_policy_for_set_branches(policy, &left_tables, &right_tables)?;
        left_policies.extend(left_split);
        right_policies.extend(right_split);
    }

    Some((left_policies, right_policies))
}

pub fn policy_requires_set_split(
    policy: &PolicyIr,
    left_tables: &HashSet<TableKey>,
    right_tables: &HashSet<TableKey>,
) -> bool {
    let sources = policy_source_keys(policy.sources());
    sources.len() > 1
        && sources
            .iter()
            .all(|source| left_tables.contains(source) || right_tables.contains(source))
        && (!sources.iter().all(|source| left_tables.contains(source))
            || !sources.iter().all(|source| right_tables.contains(source)))
}

pub fn split_policy_for_set_branches(
    policy: &PolicyIr,
    left_tables: &HashSet<TableKey>,
    right_tables: &HashSet<TableKey>,
) -> Option<(Vec<PolicyIr>, Vec<PolicyIr>)> {
    let PolicyIr::CompatDfc {
        sources,
        required_sources,
        dimensions,
        sink,
        sink_alias,
        constraint,
        on_fail,
        description,
    } = policy
    else {
        return None;
    };
    if sink.is_some() || !required_sources.is_empty() || !dimensions.is_empty() {
        return None;
    }
    if !matches!(
        on_fail,
        Resolution::Remove | Resolution::Kill | Resolution::Llm
    ) {
        return None;
    }

    let policy_sources = policy_source_keys(sources);
    let expr = parse_constraint_expr(constraint).ok()?;
    let mut left_constraints = Vec::new();
    let mut right_constraints = Vec::new();
    for conjunct in split_conjuncts(expr) {
        let refs = expr_referenced_policy_sources(&conjunct, &policy_sources);
        if refs.is_empty() {
            left_constraints.push(conjunct.clone());
            right_constraints.push(conjunct);
            continue;
        }
        if refs.iter().all(|source| left_tables.contains(source)) {
            left_constraints.push(conjunct.clone());
        }
        if refs.iter().all(|source| right_tables.contains(source)) {
            right_constraints.push(conjunct);
        }
        if refs
            .iter()
            .any(|source| !left_tables.contains(source) && !right_tables.contains(source))
            || (!refs.iter().all(|source| left_tables.contains(source))
                && !refs.iter().all(|source| right_tables.contains(source)))
        {
            return None;
        }
    }

    let make_policy = |constraints: Vec<Expr>, tables: &HashSet<TableKey>| {
        if constraints.is_empty() {
            return None;
        }
        let branch_sources = sources
            .iter()
            .filter(|source| tables.contains(&TableKey::new(source)))
            .cloned()
            .collect::<Vec<_>>();
        if branch_sources.is_empty() {
            return None;
        }
        Some(PolicyIr::CompatDfc {
            sources: branch_sources,
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: sink_alias.clone(),
            constraint: join_conjuncts(constraints).to_string(),
            on_fail: *on_fail,
            description: description.clone(),
        })
    };

    Some((
        make_policy(left_constraints, left_tables)
            .into_iter()
            .collect(),
        make_policy(right_constraints, right_tables)
            .into_iter()
            .collect(),
    ))
}

pub fn split_policy_by_source_local_conjuncts(
    policy: &PolicyIr,
    available_tables: &HashSet<TableKey>,
) -> Option<Vec<PolicyIr>> {
    let PolicyIr::CompatDfc {
        sources,
        required_sources,
        dimensions,
        sink,
        sink_alias,
        constraint,
        on_fail,
        description,
    } = policy
    else {
        return None;
    };
    if sink.is_some() || !required_sources.is_empty() || !dimensions.is_empty() {
        return None;
    }
    if !matches!(
        on_fail,
        Resolution::Remove | Resolution::Kill | Resolution::Llm
    ) {
        return None;
    }

    let policy_sources = policy_source_keys(sources);
    let expr = parse_constraint_expr(constraint).ok()?;
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
        split.push(PolicyIr::CompatDfc {
            sources: vec![source.clone()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: sink_alias.clone(),
            constraint: join_conjuncts(constraints).to_string(),
            on_fail: *on_fail,
            description: description.clone(),
        });
    }
    (!split.is_empty()).then_some(split)
}

fn table_with_joins_source_tables(table: &TableWithJoins) -> HashSet<TableKey> {
    let mut tables = table_factor_source_tables(&table.relation);
    for join in &table.joins {
        tables.extend(table_factor_source_tables(&join.relation));
    }
    tables
}

pub fn table_factor_source_tables(factor: &TableFactor) -> HashSet<TableKey> {
    match factor {
        TableFactor::Table { name, .. } => {
            HashSet::from([TableKey::from_table(&TableName::from_object_name(name))])
        }
        TableFactor::Derived { subquery, .. } => set_expr_source_tables(subquery.body.as_ref()),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => table_with_joins_source_tables(table_with_joins),
        _ => HashSet::new(),
    }
}

fn policy_source_keys(sources: &[String]) -> HashSet<TableKey> {
    sources.iter().map(|source| TableKey::new(source)).collect()
}

fn parse_constraint_expr(sql: &str) -> Result<Expr, String> {
    parse_projection_expr(sql).map_err(|err| err.to_string())
}

fn expr_referenced_policy_sources(
    expr: &Expr,
    policy_sources: &HashSet<TableKey>,
) -> HashSet<TableKey> {
    let mut refs = HashSet::new();
    collect_referenced_policy_sources(expr, policy_sources, &mut refs);
    refs
}

fn collect_referenced_policy_sources(
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

fn split_conjuncts(expr: Expr) -> Vec<Expr> {
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

fn join_conjuncts(mut conjuncts: Vec<Expr>) -> Expr {
    let first = conjuncts.remove(0);
    conjuncts
        .into_iter()
        .fold(first, |left, right| Expr::BinaryOp {
            left: Box::new(left),
            op: BinaryOperator::And,
            right: Box::new(right),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_query;
    use sqlparser::ast::Statement;

    fn select_from_sql(sql: &str) -> Select {
        let statement = parse_query(sql).expect("query should parse");
        let Statement::Query(query) = statement else {
            panic!("expected query");
        };
        match *query.body {
            SetExpr::Select(select) => *select,
            other => panic!("expected select, got {other}"),
        }
    }

    fn set_expr_from_sql(sql: &str) -> SetExpr {
        let statement = parse_query(sql).expect("query should parse");
        let Statement::Query(query) = statement else {
            panic!("expected query");
        };
        *query.body
    }

    #[test]
    fn nullable_sources_include_right_side_of_left_join() {
        let select = select_from_sql("SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id");
        let nullable = select_nullable_source_tables(&select);
        assert!(nullable.contains(&TableKey::new("foo")));
        assert!(!nullable.contains(&TableKey::new("bar")));
    }

    #[test]
    fn set_operation_detects_cross_source_policy() {
        let left = set_expr_from_sql("SELECT id FROM foo");
        let right = set_expr_from_sql("SELECT id FROM bar");
        let policy = PolicyIr::CompatDfc {
            sources: vec!["foo".to_string(), "bar".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(foo.id) > max(bar.id)".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        };
        assert!(set_operation_requires_cross_source_policies(
            &[policy],
            &left,
            &right
        ));
    }

    #[test]
    fn split_policy_by_source_local_conjuncts_for_outer_join() {
        let policy = PolicyIr::CompatDfc {
            sources: vec!["bar".to_string(), "foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(bar.id) > 1 AND max(foo.id) > 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        };
        let available = HashSet::from([TableKey::new("bar"), TableKey::new("foo")]);
        let split = split_policy_by_source_local_conjuncts(&policy, &available)
            .expect("policy should split");
        assert_eq!(split.len(), 2);
        assert!(split.iter().any(|policy| {
            policy.sources() == ["bar"] && policy.constraint().contains("max(bar.id) > 1")
        }));
        assert!(split.iter().any(|policy| {
            policy.sources() == ["foo"] && policy.constraint().contains("max(foo.id) > 1")
        }));
    }

    #[test]
    fn split_set_operation_policies_into_branch_local_policies() {
        let left = set_expr_from_sql("SELECT id FROM foo");
        let right = set_expr_from_sql("SELECT id FROM bar");
        let policy = PolicyIr::CompatDfc {
            sources: vec!["foo".to_string(), "bar".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(foo.id) > 1 AND max(bar.id) > 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        };
        let (left_policies, right_policies) =
            split_set_operation_policies(&[policy], &left, &right).expect("should split");
        assert_eq!(left_policies.len(), 1);
        assert_eq!(right_policies.len(), 1);
        assert_eq!(left_policies[0].sources(), ["foo"]);
        assert_eq!(right_policies[0].sources(), ["bar"]);
    }

    #[test]
    fn union_source_tables_include_both_branches() {
        let set_expr = set_expr_from_sql("SELECT id FROM foo UNION ALL SELECT id FROM bar");
        let tables = set_expr_source_tables(&set_expr);
        assert!(tables.contains(&TableKey::new("foo")));
        assert!(tables.contains(&TableKey::new("bar")));
    }
}
