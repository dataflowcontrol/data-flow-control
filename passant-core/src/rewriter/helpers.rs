use std::collections::{HashMap, HashSet};

use sqlparser::ast::{
    Assignment, AssignmentTarget, BinaryOperator, Expr, JoinOperator, ObjectName, Select,
    SelectItem, SetExpr, TableFactor, TableWithJoins,
};

use crate::catalog::TableCatalog;
use crate::diagnostics::RewriteError;
use crate::identifiers::TableKey;
use crate::policy::PolicyIr;
use crate::policy_store::PolicyStore;
use crate::sql::{ExprKey, qualified_column};
use crate::threshold;

use super::expr::{
    and_expr, projected_column_name, projection_expr_and_name, table_factor_base_and_alias,
};
use super::types::PolicyApplicability;

pub(crate) struct OutputColumnMapping {
    pub expr_by_column: HashMap<String, Expr>,
    pub ambiguous_columns: HashSet<String>,
}

pub(crate) fn flatten_and(expr: &Expr) -> Vec<Expr> {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            let mut conjuncts = flatten_and(left);
            conjuncts.extend(flatten_and(right));
            conjuncts
        }
        other => vec![other.clone()],
    }
}

pub(crate) fn rebuild_and(conjuncts: Vec<Expr>) -> Option<Expr> {
    if conjuncts.is_empty() {
        None
    } else {
        Some(
            conjuncts
                .into_iter()
                .reduce(and_expr)
                .expect("non-empty conjuncts"),
        )
    }
}

pub(crate) fn policy_description(policy: &PolicyIr) -> Option<&str> {
    match policy {
        PolicyIr::Pgn { description, .. } => description.as_deref(),
    }
}

pub(crate) fn direct_source_occurrence_counts(select: &Select) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for table in &select.from {
        if let Some((base, _)) = table_factor_base_and_alias(&table.relation) {
            *counts.entry(base.to_ascii_lowercase()).or_default() += 1;
        }
        for join in &table.joins {
            if let Some((base, _)) = table_factor_base_and_alias(&join.relation) {
                *counts.entry(base.to_ascii_lowercase()).or_default() += 1;
            }
        }
    }
    counts
}

pub(crate) fn table_joins_all_inner(table: &TableWithJoins) -> bool {
    !table.joins.is_empty()
        && table
            .joins
            .iter()
            .all(|join| matches!(join.join_operator, JoinOperator::Inner(_)))
}

pub(crate) fn table_with_joins_base_tables(table: &TableWithJoins) -> HashSet<TableKey> {
    let mut bases = HashSet::new();
    if let Some((base, _)) = table_factor_base_and_alias(&table.relation) {
        bases.insert(TableKey::new(&base));
    }
    for join in &table.joins {
        if let Some((base, _)) = table_factor_base_and_alias(&join.relation) {
            bases.insert(TableKey::new(&base));
        }
    }
    bases
}

pub(crate) fn prune_dominated_applicable_with_store<'a>(
    store: &PolicyStore,
    applicable: Vec<(usize, &'a PolicyIr, PolicyApplicability)>,
) -> (Vec<(usize, &'a PolicyIr, PolicyApplicability)>, usize) {
    let mut keep = vec![true; applicable.len()];
    let mut strongest_by_key: HashMap<
        threshold::ThresholdKey,
        (usize, threshold::ThresholdPredicate),
    > = HashMap::new();

    for (slot, (index, policy, applicability)) in applicable.iter().enumerate() {
        if *applicability != PolicyApplicability::Normal {
            continue;
        }
        let Some(candidate) = store
            .compiled(*index)
            .and_then(|entry| entry.threshold.clone())
            .or_else(|| threshold::threshold_predicate_from_policy(policy))
        else {
            continue;
        };
        if let Some((existing_slot, existing)) = strongest_by_key.get(&candidate.key).cloned() {
            if threshold::threshold_dominates_predicates(&existing, &candidate) {
                keep[slot] = false;
                continue;
            }
            if threshold::threshold_dominates_predicates(&candidate, &existing) {
                keep[existing_slot] = false;
                strongest_by_key.insert(candidate.key.clone(), (slot, candidate));
            }
        } else {
            strongest_by_key.insert(candidate.key.clone(), (slot, candidate));
        }
    }

    let dominated = keep.iter().filter(|&&k| !k).count();
    let kept = applicable
        .into_iter()
        .enumerate()
        .filter(|(slot, _)| keep[*slot])
        .map(|(_, item)| item)
        .collect();
    (kept, dominated)
}

#[allow(dead_code)]
pub(crate) fn prune_dominated_remove_policies(
    applicable: Vec<(&PolicyIr, PolicyApplicability)>,
) -> Vec<(&PolicyIr, PolicyApplicability)> {
    let indexed = applicable
        .into_iter()
        .enumerate()
        .map(|(index, (policy, applicability))| (index, policy, applicability))
        .collect::<Vec<_>>();
    let store = PolicyStore::default();
    prune_dominated_applicable_with_store(&store, indexed)
        .0
        .into_iter()
        .map(|(_, policy, applicability)| (policy, applicability))
        .collect()
}

pub(crate) const OUTPUT_MARKER: &str = "_OUTPUT_";

pub(crate) fn insert_select_mapping(
    insert: &sqlparser::ast::Insert,
) -> Result<OutputColumnMapping, RewriteError> {
    let mut mapping = OutputColumnMapping {
        expr_by_column: HashMap::new(),
        ambiguous_columns: HashSet::new(),
    };
    let Some(query) = insert.source.as_ref() else {
        return Ok(mapping);
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Ok(mapping);
    };

    for (index, item) in select.projection.iter().enumerate() {
        let Some((expr, alias)) = projection_expr_and_name(item) else {
            continue;
        };
        if let Some(column) = insert.columns.get(index) {
            insert_output_mapping(&mut mapping, column.value.clone(), expr.clone())?;
        }
        if let Some(alias) = alias {
            mapping
                .expr_by_column
                .entry(alias.to_ascii_lowercase())
                .or_insert_with(|| expr.clone());
        }
    }
    Ok(mapping)
}

pub(crate) fn select_output_column_mapping(
    select: &Select,
    catalog: &TableCatalog,
) -> Result<OutputColumnMapping, RewriteError> {
    let mut mapping = OutputColumnMapping {
        expr_by_column: HashMap::new(),
        ambiguous_columns: HashSet::new(),
    };
    for item in &select.projection {
        match item {
            SelectItem::UnnamedExpr(expr) => {
                let Some(name) = projected_column_name(expr) else {
                    continue;
                };
                insert_output_mapping(&mut mapping, name, expr.clone())?;
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                insert_output_mapping(&mut mapping, alias.value.clone(), expr.clone())?;
            }
            SelectItem::Wildcard(_) => {
                expand_wildcard_output_columns(select, catalog, None, &mut mapping)?;
            }
            SelectItem::QualifiedWildcard(object_name, _) => {
                expand_wildcard_output_columns(select, catalog, Some(object_name), &mut mapping)?;
            }
        }
    }
    Ok(mapping)
}

fn insert_output_mapping(
    mapping: &mut OutputColumnMapping,
    name: String,
    expr: Expr,
) -> Result<(), RewriteError> {
    let key = name.to_ascii_lowercase();
    if mapping.ambiguous_columns.contains(&key) {
        return Ok(());
    }
    if let Some(existing) = mapping.expr_by_column.get(&key) {
        if ExprKey::from_expr(existing) != ExprKey::from_expr(&expr) {
            mapping.ambiguous_columns.insert(key.clone());
            mapping.expr_by_column.remove(&key);
        }
        return Ok(());
    }
    mapping.expr_by_column.insert(key, expr);
    Ok(())
}

fn expand_wildcard_output_columns(
    select: &Select,
    catalog: &TableCatalog,
    qualified_table: Option<&ObjectName>,
    mapping: &mut OutputColumnMapping,
) -> Result<(), RewriteError> {
    let mut expanded = false;
    for table in &select.from {
        expand_wildcard_from_table_factor(
            &table.relation,
            catalog,
            qualified_table,
            mapping,
            &mut expanded,
        )?;
        for join in &table.joins {
            expand_wildcard_from_table_factor(
                &join.relation,
                catalog,
                qualified_table,
                mapping,
                &mut expanded,
            )?;
        }
    }
    if !expanded {
        return Ok(());
    }
    Ok(())
}

fn expand_wildcard_from_table_factor(
    factor: &TableFactor,
    catalog: &TableCatalog,
    qualified_table: Option<&ObjectName>,
    mapping: &mut OutputColumnMapping,
    expanded: &mut bool,
) -> Result<(), RewriteError> {
    let Some((base, alias)) = table_factor_base_and_alias(factor) else {
        return Ok(());
    };
    if let Some(object_name) = qualified_table
        && !base.eq_ignore_ascii_case(&object_name.to_string())
    {
        return Ok(());
    }
    let Some(columns) = catalog.columns(&base) else {
        return Ok(());
    };
    let table_ref = alias.as_deref().unwrap_or(&base);
    for column in columns {
        insert_output_mapping(mapping, column.clone(), qualified_column(table_ref, column))?;
    }
    *expanded = true;
    Ok(())
}

pub(crate) fn update_target_name(table: &TableWithJoins) -> Option<String> {
    match &table.relation {
        TableFactor::Table { name, .. } => Some(name.to_string()),
        _ => None,
    }
}

pub(crate) fn update_assignment_mapping(assignments: &[Assignment]) -> HashMap<String, Expr> {
    let mut mapping = HashMap::new();
    for assignment in assignments {
        let AssignmentTarget::ColumnName(name) = &assignment.target else {
            continue;
        };
        if let Some(column) = name.0.last() {
            mapping.insert(column.value.to_ascii_lowercase(), assignment.value.clone());
        }
    }
    mapping
}

pub(crate) fn update_output_column_mapping(
    table: &TableWithJoins,
    assignments: &[Assignment],
    catalog: &TableCatalog,
) -> Result<OutputColumnMapping, RewriteError> {
    let mut mapping = OutputColumnMapping {
        expr_by_column: update_assignment_mapping(assignments),
        ambiguous_columns: HashSet::new(),
    };
    let Some(table_name) = update_target_name(table) else {
        return Ok(mapping);
    };
    let table_ref = table_name.as_str();
    if let Some(columns) = catalog.columns(&table_name) {
        for column in columns {
            let key = column.to_ascii_lowercase();
            mapping
                .expr_by_column
                .entry(key)
                .or_insert_with(|| qualified_column(table_ref, column));
        }
    }
    Ok(mapping)
}
