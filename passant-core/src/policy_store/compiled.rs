use std::collections::HashSet;
use std::sync::Arc;

use smallvec::SmallVec;
use sqlparser::ast::Expr;

use super::PolicyStore;
use crate::identifiers::{ColumnKey, TableKey};
use crate::policy::{PolicyIr, Resolution};
use crate::policy_compile::{ParsedPolicyConstraint, parse_policy_constraint};
use crate::rewriter::dimensions::{DimensionJoinPlan, compile_dimension_join_plan};
use crate::rewriter::preprocess_policy_constraint;
use crate::semiring::{AggregateAnalysis, SemiringAnalysis, analyze_constraint_expr};
use crate::source_sets::{
    compile_constraint_referenced_source_keys, compile_source_local_conjuncts,
};
use crate::sql::collect_qualified_columns_from_expr;
use crate::threshold::{ThresholdPredicate, threshold_predicate_from_policy};

/// Parsed constraint metadata compiled once at policy registration.
#[derive(Debug, Clone)]
pub struct CompiledExpr {
    pub source_sql: Arc<str>,
    pub ast: Expr,
}

/// Registration-time compiled policy used by the rewrite path.
#[derive(Debug, Clone)]
pub struct CompiledPolicy {
    pub index: usize,
    pub policy: PolicyIr,
    pub active: bool,
    pub constraint: Option<CompiledExpr>,
    pub semiring: SemiringAnalysis,
    pub source_keys: SmallVec<[TableKey; 4]>,
    pub required_source_keys: SmallVec<[TableKey; 4]>,
    pub sink_key: Option<TableKey>,
    pub(crate) threshold: Option<ThresholdPredicate>,
    pub join_pushdown_eligible: bool,
    /// Precomputed `transform_scan_aggregates` for distributive aggregate constraints.
    pub(crate) scan_ready_expr: Option<Expr>,
    /// Pre-split AND conjuncts keyed by source for multi-source enforcement policies.
    pub(crate) source_local_conjuncts: Option<SmallVec<[(TableKey, Expr); 4]>>,
    /// Source tables referenced in the constraint expression.
    pub(crate) constraint_referenced_sources: SmallVec<[TableKey; 4]>,
    /// Qualified columns referenced in the constraint/dimensions, interned at registration.
    pub(crate) constraint_referenced_columns: SmallVec<[(TableKey, ColumnKey); 4]>,
    /// Cached dimension equalities from the constraint AST (registration-time).
    pub(crate) dimension_join_plan: Option<DimensionJoinPlan>,
}

pub(crate) fn is_enforcement_resolution(resolution: Resolution) -> bool {
    matches!(
        resolution,
        Resolution::Remove
            | Resolution::Kill
            | Resolution::Udf(_)
            | Resolution::RelationUdf(_)
            | Resolution::Ui
    )
}

impl PolicyStore {
    pub(crate) fn compile_policy(
        &mut self,
        index: usize,
        mut policy: PolicyIr,
        parsed: Option<ParsedPolicyConstraint>,
    ) -> CompiledPolicy {
        let PolicyIr::Pgn { constraint, .. } = &mut policy;
        let parsed = parsed.or_else(|| parse_policy_constraint(constraint).ok());
        if let Some(ref draft) = parsed {
            *constraint = draft.sql.clone();
        } else {
            *constraint = preprocess_policy_constraint(constraint);
        }
        let constraint_sql = self.intern_string(policy.constraint());
        let constraint = parsed.as_ref().map(|draft| CompiledExpr {
            source_sql: constraint_sql.clone(),
            ast: draft.expr.clone(),
        });
        let semiring = parsed
            .as_ref()
            .map(|draft| draft.semiring.clone())
            .unwrap_or_else(|| semiring_for_constraint(constraint_sql.as_ref()));
        let source_keys = policy
            .sources()
            .iter()
            .map(|source| self.intern_table_key(source))
            .collect::<SmallVec<[TableKey; 4]>>();
        let required_source_keys = policy
            .required_sources()
            .iter()
            .map(|source| self.intern_table_key(source))
            .collect::<SmallVec<[TableKey; 4]>>();
        let sink_key = policy.sink().map(|sink| self.intern_table_key(sink));
        let threshold = parsed
            .as_ref()
            .and_then(|draft| draft.threshold.clone())
            .or_else(|| threshold_predicate_from_policy(&policy));
        let join_pushdown_eligible = matches!(
            &policy,
            PolicyIr::Pgn {
                sources,
                required_sources,
                sink: None,
                on_fail: Resolution::Remove,
                ..
            } if required_sources.is_empty() && sources.len() == 1
        );
        let (source_local_conjuncts, constraint_referenced_sources, constraint_referenced_columns) =
            if let Some(compiled) = constraint.as_ref() {
                let referenced =
                    compile_constraint_referenced_source_keys(&compiled.ast, &source_keys);
                let columns = dedup_referenced_column_keys(
                    compile_constraint_referenced_column_keys(self, &compiled.ast),
                );
                let conjuncts =
                    if matches!(policy.resolution(), Resolution::Remove | Resolution::Kill)
                        && policy.sink().is_none()
                        && policy.required_sources().is_empty()
                        && policy.dimension_tables().is_empty()
                        && policy.dimension_queries().is_empty()
                        && source_keys.len() > 1
                    {
                        compile_source_local_conjuncts(&compiled.ast, &source_keys)
                    } else {
                        None
                    };
                (conjuncts, referenced, columns)
            } else {
                (None, SmallVec::new(), SmallVec::new())
            };
        let dimension_join_plan = constraint.as_ref().and_then(|compiled| {
            if policy.dimension_tables().is_empty() && policy.dimension_queries().is_empty() {
                return None;
            }
            let source_key_set: HashSet<_> = source_keys.iter().cloned().collect();
            compile_dimension_join_plan(
                &compiled.ast,
                policy.dimension_tables(),
                policy.dimension_aliases(),
                &source_key_set,
            )
        });

        CompiledPolicy {
            index,
            policy,
            active: true,
            constraint,
            semiring,
            source_keys,
            required_source_keys,
            sink_key,
            threshold,
            join_pushdown_eligible,
            scan_ready_expr: None,
            source_local_conjuncts,
            constraint_referenced_sources,
            constraint_referenced_columns,
            dimension_join_plan,
        }
    }
}

pub(crate) fn compile_branch_policy(
    store: &mut PolicyStore,
    index: usize,
    mut policy: PolicyIr,
    constraint_ast: Expr,
) -> CompiledPolicy {
    let PolicyIr::Pgn { constraint, .. } = &mut policy;
    *constraint = preprocess_policy_constraint(constraint);
    let constraint_sql = store.intern_string(policy.constraint());
    let constraint = CompiledExpr {
        source_sql: constraint_sql,
        ast: constraint_ast.clone(),
    };
    let semiring = semiring_for_constraint_expr(&constraint_ast);
    let source_keys = policy
        .sources()
        .iter()
        .map(|source| store.intern_table_key(source))
        .collect::<SmallVec<[TableKey; 4]>>();
    let required_source_keys = policy
        .required_sources()
        .iter()
        .map(|source| store.intern_table_key(source))
        .collect::<SmallVec<[TableKey; 4]>>();
    let sink_key = policy.sink().map(|sink| store.intern_table_key(sink));
    let threshold = threshold_predicate_from_policy(&policy);
    let join_pushdown_eligible = matches!(
        &policy,
        PolicyIr::Pgn {
            sources,
            required_sources,
            sink: None,
            on_fail: Resolution::Remove,
            ..
        } if required_sources.is_empty() && sources.len() == 1
    );
    let referenced = compile_constraint_referenced_source_keys(&constraint_ast, &source_keys);
    let columns = dedup_referenced_column_keys(compile_constraint_referenced_column_keys(
        store,
        &constraint_ast,
    ));
    let source_local_conjuncts =
        if matches!(policy.resolution(), Resolution::Remove | Resolution::Kill)
            && policy.sink().is_none()
            && policy.required_sources().is_empty()
            && policy.dimension_tables().is_empty()
            && policy.dimension_queries().is_empty()
            && source_keys.len() > 1
        {
            compile_source_local_conjuncts(&constraint_ast, &source_keys)
        } else {
            None
        };
    let dimension_join_plan =
        if policy.dimension_tables().is_empty() && policy.dimension_queries().is_empty() {
            None
        } else {
            let source_key_set: HashSet<_> = source_keys.iter().cloned().collect();
            compile_dimension_join_plan(
                &constraint_ast,
                policy.dimension_tables(),
                policy.dimension_aliases(),
                &source_key_set,
            )
        };

    CompiledPolicy {
        index,
        policy,
        active: true,
        constraint: Some(constraint),
        semiring,
        source_keys,
        required_source_keys,
        sink_key,
        threshold,
        join_pushdown_eligible,
        scan_ready_expr: None,
        source_local_conjuncts,
        constraint_referenced_sources: referenced,
        constraint_referenced_columns: columns,
        dimension_join_plan,
    }
}

fn compile_constraint_referenced_column_keys(
    store: &mut PolicyStore,
    expr: &Expr,
) -> SmallVec<[(TableKey, ColumnKey); 4]> {
    let mut pairs = SmallVec::new();
    for column in collect_qualified_columns_from_expr(expr) {
        let table = store.intern_table_key(column.table.as_str());
        let column_key = store.intern_column_key(column.column.as_str());
        pairs.push((table, column_key));
    }
    pairs
}

fn dedup_referenced_column_keys(
    pairs: SmallVec<[(TableKey, ColumnKey); 4]>,
) -> SmallVec<[(TableKey, ColumnKey); 4]> {
    let mut seen = HashSet::new();
    let mut deduped: SmallVec<[(TableKey, ColumnKey); 4]> = SmallVec::new();
    for (table, column) in pairs {
        if seen.insert((table.clone(), column.clone())) {
            deduped.push((table, column));
        }
    }
    deduped.sort_by(|left, right| {
        left.0
            .as_str()
            .cmp(right.0.as_str())
            .then_with(|| left.1.as_str().cmp(right.1.as_str()))
    });
    deduped
}

fn semiring_for_constraint(constraint: &str) -> SemiringAnalysis {
    match crate::semiring::analyze_constraint(constraint) {
        Ok(aggregates) => merge_semiring_from_aggregates(aggregates),
        Err(_) => SemiringAnalysis {
            aggregate_count: 0,
            all_distributive: false,
            non_distributive_aggregates: vec![format!("unparseable::{constraint}")],
        },
    }
}

fn semiring_for_constraint_expr(expr: &Expr) -> SemiringAnalysis {
    merge_semiring_from_aggregates(analyze_constraint_expr(expr))
}

pub(crate) fn merge_semiring<'a>(
    entries: impl Iterator<Item = &'a CompiledPolicy>,
) -> SemiringAnalysis {
    let mut result = SemiringAnalysis::default();
    for entry in entries {
        result.aggregate_count += entry.semiring.aggregate_count;
        if !entry.semiring.all_distributive {
            result.all_distributive = false;
        }
        result
            .non_distributive_aggregates
            .extend(entry.semiring.non_distributive_aggregates.iter().cloned());
    }
    result
}

fn merge_semiring_from_aggregates(aggregates: Vec<AggregateAnalysis>) -> SemiringAnalysis {
    let mut non_distributive_aggregates = Vec::new();
    let mut all_distributive = true;
    for aggregate in &aggregates {
        if !aggregate.distributive {
            all_distributive = false;
            non_distributive_aggregates.push(aggregate.expression.clone());
        }
    }
    SemiringAnalysis {
        aggregate_count: aggregates.len(),
        all_distributive,
        non_distributive_aggregates,
    }
}
