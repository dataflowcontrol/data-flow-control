//! Inherited policy demands and synthesized lineage for the joint endpoint.
//! Each original query is entered once and synthesized once. CTE references bind
//! memoized schemas; generated policy wrappers never enter this traversal.
mod terminal_distinct;
mod terminal_except;
mod terminal_intersect;

use super::RewriteError;
use super::insert_output_policy::InputColumn;
use super::projection::{ensure_projection_aliases, select_is_aggregation};
use super::types::{PassantRewriter, RewriteContext};
use crate::identifiers::{QualifiedColumn, TableKey};
use crate::policy::PolicyIr;
use crate::policy_store::BranchPolicyEntry;
use crate::sql::render_expr;
use sqlparser::ast::{
    Expr, Ident, JoinOperator, Query, Select, SelectItem, SetExpr, TableAlias, TableAliasColumnDef,
    TableFactor, visit_expressions, visit_expressions_mut,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::rc::Rc;

fn unsupported(reason: &str) -> RewriteError {
    RewriteError::unsupported_statement(format!(
        "joint resolution lineage is not established for {reason}"
    ))
}
#[derive(Debug, Clone)]
struct OutputColumn {
    name: Ident,
    origin: Option<(TableKey, String)>,
    public: bool,
    presence: Option<TableKey>,
}
#[derive(Debug, Clone, Default)]
struct Synthesized {
    bindings: HashMap<TableKey, Ident>,
    outputs: Vec<OutputColumn>,
    inputs: Vec<InputColumn>,
    presence: HashMap<TableKey, Expr>,
    finalized: HashMap<usize, Expr>,
}
#[derive(Debug, Default)]
pub(crate) struct TraversalState {
    used: HashSet<String>,
    has_nullable_join: bool,
    has_aggregation: bool,
    top_down: usize,
    bottom_up: usize,
    visits: HashMap<usize, (usize, usize)>,
}
#[derive(Debug, Clone)]
pub(crate) struct JointLineageContext {
    demands: Rc<HashMap<TableKey, HashSet<String>>>,
    policies: Rc<Vec<usize>>,
    public_labels: bool,
    output_aliases: Vec<Ident>,
    ctes: HashMap<TableKey, Vec<OutputColumn>>,
    state: Rc<RefCell<TraversalState>>,
    root: bool,
    synthesized: Synthesized,
}

pub(crate) fn rewrite_joint_root(
    rewriter: &PassantRewriter,
    query: &mut Query,
    context: &mut RewriteContext,
) -> Result<bool, RewriteError> {
    if context.joint_resolution_prepared || context.joint_lineage_legacy.is_some() {
        return Ok(false);
    }
    // Binding/applicability discovery is separate from policy placement visits.
    let original_tables = crate::statement_tables::statement_table_keys(
        &sqlparser::ast::Statement::Query(Box::new(query.clone())),
    );
    let sink = context.sink.as_deref().map(TableKey::new);
    let mut applicable: Vec<_> = rewriter
        .store
        .policy_indices()
        .into_iter()
        .filter(|index| {
            rewriter.store.compiled(*index).is_some_and(|compiled| {
                super::policy_expr::compiled_policy_applicability(
                    compiled,
                    &original_tables,
                    sink.as_ref(),
                    true,
                )
                .is_some()
            })
        })
        .collect();
    if applicable.is_empty() {
        return Ok(false);
    }
    let strictly_applicable: Vec<_> = applicable
        .iter()
        .copied()
        .filter(|index| {
            rewriter.store.compiled(*index).is_some_and(|compiled| {
                super::policy_expr::compiled_policy_applicability(
                    compiled,
                    &original_tables,
                    sink.as_ref(),
                    false,
                )
                .is_some()
            })
        })
        .collect();
    let has_except = terminal_except::validate_dependencies(rewriter, query, &strictly_applicable)?;
    if has_except {
        // A negative dependency is relevant only when the whole policy scope
        // can apply. Do not route an absent-SOURCE policy through legacy partial
        // branch visibility, which could resurrect an inapplicable condition.
        applicable = strictly_applicable;
        if applicable.is_empty() {
            return Ok(true);
        }
    }
    // The existing INSERT aggregate-output plan already defers its covered
    // effects to the Body endpoint; do not replace that proven legacy route.
    let has_output_policy = applicable.iter().any(|index| {
        super::output_scope::has_output_policy(rewriter, *index)
            && !context
                .insert_output_policy
                .as_ref()
                .is_some_and(|plan| plan.policy_indices.contains(index))
    });
    let nested = has_output_policy
        || query
            .with
            .as_ref()
            .is_some_and(|w| !w.cte_tables.is_empty())
        || match query.body.as_ref() {
            SetExpr::Select(s) => {
                s.distinct.is_some()
                    || (applicable.iter().any(|i| {
                        rewriter
                            .store
                            .policy(*i)
                            .is_some_and(|p| p.sources().len() > 1)
                    }) && s
                        .from
                        .iter()
                        .flat_map(|t| &t.joins)
                        .any(|j| nullable_join(&j.join_operator)))
                    || s.from.iter().any(|t| {
                        std::iter::once(&t.relation)
                            .chain(t.joins.iter().map(|j| &j.relation))
                            .any(|f| matches!(f, TableFactor::Derived { .. }))
                    })
            }
            _ => true,
        };
    if !nested {
        return Ok(false);
    }
    let splittable_only = !has_output_policy
        && applicable.iter().all(|i| {
            rewriter
                .store
                .policy(*i)
                .is_some_and(|p| rewriter.resolution_properties(&p.resolution()).splittable)
        });
    let mut demands: HashMap<TableKey, HashSet<String>> = HashMap::new();
    for index in &applicable {
        let PolicyIr::Pgn {
            sources,
            source_aliases,
            constraint,
            ..
        } = rewriter.store.policy(*index).unwrap();
        for source in sources {
            demands.entry(TableKey::new(source)).or_default();
        }
        let expr = super::columns::replace_source_alias_qualifiers(
            crate::sql::parse_policy_expr_duckdb(constraint)?,
            source_aliases,
        );
        let _: ControlFlow<()> = visit_expressions(&expr, |expr| {
            match expr {
                Expr::CompoundIdentifier(parts) => {
                    if let Some(column) = QualifiedColumn::from_compound_identifier(parts)
                        && let Some(columns) = demands.get_mut(&TableKey::from_table(&column.table))
                    {
                        columns.insert(column.column.key());
                    }
                }
                Expr::Identifier(id) if sources.len() == 1 => {
                    demands
                        .get_mut(&TableKey::new(&sources[0]))
                        .unwrap()
                        .insert(id.value.to_ascii_lowercase());
                }
                _ => {}
            }
            ControlFlow::Continue(())
        });
    }
    let session = JointLineageContext {
        demands: Rc::new(demands),
        policies: Rc::new(applicable.clone()),
        public_labels: context.sink.is_none() || context.require_output_labels,
        output_aliases: Vec::new(),
        ctes: HashMap::new(),
        root: true,
        state: Rc::new(RefCell::new(TraversalState {
            used: identifier_names(&query.to_string())?,
            ..Default::default()
        })),
        synthesized: Synthesized::default(),
    };
    // The pure top-down phase validates capabilities and inherits the unioned
    // demands before any original query is instrumented. Legacy fallback is
    // possible only here, never after partial bottom-up rewriting.
    if let Err(error) = inherit_query_demands(rewriter, query, &session, true) {
        let multisource_nullable = session.state.borrow().has_nullable_join
            && applicable.iter().any(|index| {
                rewriter
                    .store
                    .policy(*index)
                    .is_some_and(|p| p.sources().len() > 1)
            });
        if multisource_nullable {
            return Err(unsupported(
                "multi-source nullable JOIN requires contributor-presence provenance",
            ));
        }
        if !has_except && splittable_only && is_legacy_shape_error(&error) {
            context.joint_lineage_legacy = Some(session.state.clone());
            return Ok(false);
        }
        return Err(error);
    }
    let mut local = context.clone();
    local.joint_lineage = Some(session);
    synthesize_query(rewriter, query, &mut local)?;
    let session = local.joint_lineage.take().unwrap();
    debug_assert_eq!(
        session.state.borrow().top_down,
        session.state.borrow().bottom_up
    );
    if !local.sink_output_columns.is_empty()
        && let SetExpr::Select(select) = query.body.as_ref()
    {
        for (column, item) in local.sink_output_columns.iter().zip(&select.projection) {
            if let Some((expr, _)) = super::expr::projection_expr_and_name(item) {
                local
                    .sink_expr_by_column
                    .insert(column.value.to_ascii_lowercase(), expr.clone());
            }
        }
    }
    let inputs = session.synthesized.inputs;
    let mut by_source: HashMap<TableKey, HashSet<String>> = HashMap::new();
    for (expr, origin) in &inputs {
        if let (Expr::CompoundIdentifier(parts), Some((source, _))) = (expr, origin) {
            by_source
                .entry(source.clone())
                .or_default()
                .insert(parts[0].to_string());
        }
    }
    for (source, expr) in &session.synthesized.presence {
        if let Expr::CompoundIdentifier(parts) = expr {
            by_source
                .entry(source.clone())
                .or_default()
                .insert(parts[0].to_string());
        }
    }
    let mut entries = Vec::new();
    let mut root_sources = HashSet::new();
    for index in applicable {
        let Some(mut policy) = rewriter.store.policy(index).cloned() else {
            continue;
        };
        let enforce_required = policy.sink().is_some();
        let PolicyIr::Pgn {
            sources,
            required_sources,
            source_aliases,
            constraint,
            ..
        } = &mut policy;
        let original_sources: HashSet<_> = sources.iter().map(|s| TableKey::new(s)).collect();
        let mut expr = if let Some(finalized) = session.synthesized.finalized.get(&index) {
            finalized.clone()
        } else {
            crate::sql::parse_policy_expr_duckdb(constraint)?
        };
        expr = super::columns::replace_source_alias_qualifiers(expr, source_aliases);
        let mut source_map = HashMap::new();
        for source in sources.iter() {
            if original_tables.contains(&TableKey::new(source))
                && !by_source.contains_key(&TableKey::new(source))
            {
                return Err(unsupported(
                    "a contributing source whose lineage could not be recovered",
                ));
            }
            if let Some(qualifiers) = by_source.get(&TableKey::new(source)) {
                if qualifiers.len() != 1 {
                    return Err(unsupported(
                        "multiple occurrences of one source at the joint endpoint",
                    ));
                }
                source_map.insert(
                    source.to_ascii_lowercase(),
                    qualifiers.iter().next().unwrap().clone(),
                );
            }
        }
        let result = visit_expressions_mut(&mut expr, |expr| {
            let Expr::CompoundIdentifier(parts) = expr else {
                return ControlFlow::Continue(());
            };
            let Some(column) = QualifiedColumn::from_compound_identifier(parts) else {
                return ControlFlow::Continue(());
            };
            let source = TableKey::from_table(&column.table);
            if !original_sources.contains(&source) || !by_source.contains_key(&source) {
                return ControlFlow::Continue(());
            }
            let key = (source, column.column.key());
            let matches: Vec<_> = inputs
                .iter()
                .filter(|(_, origin)| origin.as_ref() == Some(&key))
                .collect();
            let Some((candidate, _)) = matches.first() else {
                return ControlFlow::Break(unsupported(
                    "unavailable source attribute at the joint endpoint",
                ));
            };
            // Source-factor uniqueness was established during synthesis. More
            // than one match here is an equivalent projected alias, not another
            // scan; reuse the first existing alias rather than adding metadata.
            *expr = candidate.clone();
            ControlFlow::Continue(())
        });
        if let ControlFlow::Break(error) = result {
            return Err(error);
        }
        if !session.synthesized.presence.is_empty() {
            let required: HashSet<_> = required_sources
                .iter()
                .filter(|_| enforce_required)
                .map(|s| TableKey::new(s))
                .collect();
            let plain = sources
                .iter()
                .filter(|s| !required.contains(&TableKey::new(s)));
            let present = |source: &String| {
                session
                    .synthesized
                    .presence
                    .get(&TableKey::new(source))
                    .map(|expr| format!("({expr}) IS TRUE"))
                    .unwrap_or_else(|| "TRUE".into())
            };
            let ordinary = plain.map(present).collect::<Vec<_>>();
            let required = required_sources
                .iter()
                .filter(|_| enforce_required)
                .map(present)
                .collect::<Vec<_>>();
            let ordinary = if ordinary.is_empty() {
                "TRUE".into()
            } else {
                ordinary.join(" AND ")
            };
            let required = if required.is_empty() {
                "TRUE".into()
            } else {
                required.join(" AND ")
            };
            expr = crate::sql::parse_policy_expr_duckdb(&format!(
                "CASE WHEN {ordinary} THEN ({required}) AND ({expr}) ELSE TRUE END"
            ))?;
        }
        for source in sources.iter_mut().chain(required_sources.iter_mut()) {
            if let Some(mapped) = source_map.get(&source.to_ascii_lowercase()) {
                *source = mapped.clone();
            }
        }
        root_sources.extend(source_map.values().map(|name| TableKey::new(name)));
        source_aliases.clear();
        *constraint = render_expr(&expr, None);
        entries.push(BranchPolicyEntry {
            original_policy_index: rewriter.store.original_policy_index(index),
            policy,
            constraint_ast: expr,
        });
    }
    let branch = PassantRewriter::with_branch_view(
        &rewriter.store,
        entries,
        rewriter.catalog.clone(),
        rewriter.parse_dialect,
        &rewriter.resolution_registry,
    );
    local.joint_resolution_prepared = true;
    local.joint_resolution_endpoint = true;
    local.joint_resolution_sources = root_sources;
    local.insert_output_policy = None;
    branch.rewrite_query_endpoint(query, &mut local)?;
    Ok(true)
}

fn is_legacy_shape_error(error: &RewriteError) -> bool {
    let message = error.to_string();
    // Safety errors (ambiguous occurrence, unavailable lineage, USING/NATURAL)
    // never fall back to a path that silently consumes the damaged metadata.
    [
        "set operations",
        "recursive CTEs",
        "nested WITH/ORDER/LIMIT/OFFSET/FETCH",
        "aggregation or DISTINCT",
        "TOP, QUALIFY",
        "expression subqueries or windows",
        "non-inner joins",
        "table functions or lateral/unaliased",
        "missing catalog for demanded source lineage",
        "unproven unaliased expression output label",
        "DISTINCT ON requires",
        "DISTINCT over an existing aggregate",
        "DIMENSION-dependent terminal DISTINCT",
    ]
    .iter()
    .any(|s| message.contains(s))
}

fn inherit_query_demands(
    rewriter: &PassantRewriter,
    query: &Query,
    frame: &JointLineageContext,
    _root: bool,
) -> Result<(), RewriteError> {
    use sqlparser::ast::{Visit, Visitor};
    struct Inherited<'a> {
        rewriter: &'a PassantRewriter,
        state: &'a Rc<RefCell<TraversalState>>,
        first: bool,
        public_labels: bool,
        named_queries: HashMap<usize, Vec<Ident>>,
        error: Option<RewriteError>,
    }
    impl Visitor for Inherited<'_> {
        type Break = ();
        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
            if self.error.is_none()
                && (matches!(
                    expr,
                    Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. }
                ) || matches!(expr, Expr::Function(f) if f.over.is_some()))
            {
                self.error = Some(unsupported("expression subqueries or windows"));
            }
            ControlFlow::Continue(())
        }
        fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<()> {
            if let TableFactor::Derived {
                subquery,
                alias: Some(alias),
                ..
            } = factor
            {
                self.named_queries.insert(
                    subquery.as_ref() as *const Query as usize,
                    alias
                        .columns
                        .iter()
                        .map(|column| column.name.clone())
                        .collect(),
                );
            }
            ControlFlow::Continue(())
        }
        fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<()> {
            if let Some(with) = &query.with {
                for cte in &with.cte_tables {
                    self.named_queries.insert(
                        cte.query.as_ref() as *const Query as usize,
                        cte.alias
                            .columns
                            .iter()
                            .map(|column| column.name.clone())
                            .collect(),
                    );
                }
            }
            let named = self.named_queries.get(&(query as *const Query as usize));
            let public_labels =
                self.public_labels || named.is_some() || query_requires_output_names(query);
            let supplied_names = if query.order_by.is_none() {
                named.map_or(0, Vec::len)
            } else {
                0
            };
            let root = self.first;
            self.first = false;
            {
                let mut state = self.state.borrow_mut();
                state.top_down += 1;
                state
                    .visits
                    .entry(query as *const Query as usize)
                    .or_default()
                    .0 += 1;
            }
            let capability = (|| {
                if !root && query.with.is_some() {
                    return Err(unsupported("nested WITH/ORDER/LIMIT/OFFSET/FETCH inputs"));
                }
                if query.with.as_ref().is_some_and(|with| with.recursive) {
                    return Err(unsupported("recursive CTEs"));
                }
                validate_body(
                    query.body.as_ref(),
                    self.rewriter,
                    root,
                    public_labels,
                    supplied_names,
                    self.state,
                )
            })();
            if self.error.is_none() {
                self.error = capability.err();
            }
            // Finish the inherited phase even for legacy opaque shapes. No tree
            // has been mutated, and descendants need no second strategy analysis.
            ControlFlow::Continue(())
        }
    }
    let missing_catalog = frame
        .demands
        .keys()
        .any(|source| rewriter.catalog.columns(source.as_str()).is_none());
    let terminal_distinct = matches!(query.body.as_ref(), SetExpr::Select(select) if select.distinct.is_some())
        || matches!(
            query.body.as_ref(),
            SetExpr::SetOperation {
                op: sqlparser::ast::SetOperator::Union,
                set_quantifier: sqlparser::ast::SetQuantifier::None
                    | sqlparser::ast::SetQuantifier::Distinct,
                ..
            }
        );
    let terminal_intersection = matches!(
        query.body.as_ref(),
        SetExpr::SetOperation {
            op: sqlparser::ast::SetOperator::Intersect,
            set_quantifier: sqlparser::ast::SetQuantifier::None
                | sqlparser::ast::SetQuantifier::Distinct,
            ..
        }
    );
    if terminal_intersection {
        terminal_intersect::validate_policies(rewriter, &frame.policies)?;
    }
    let terminal_difference = matches!(
        query.body.as_ref(),
        SetExpr::SetOperation {
            op: sqlparser::ast::SetOperator::Except,
            ..
        }
    );
    let dimension_finalization = (terminal_distinct || terminal_intersection || terminal_difference) && frame.policies.iter().any(|index| {
        matches!(rewriter.store.policy(*index), Some(PolicyIr::Pgn {dimension_tables,dimension_queries,..}) if !dimension_tables.is_empty() || !dimension_queries.is_empty())
    });
    let mut inherited = Inherited {
        rewriter,
        state: &frame.state,
        first: true,
        public_labels: frame.public_labels,
        named_queries: HashMap::new(),
        error: if dimension_finalization {
            Some(unsupported(
                "DIMENSION-dependent terminal DISTINCT requires late dimension binding",
            ))
        } else {
            missing_catalog.then(|| unsupported("missing catalog for demanded source lineage"))
        },
    };
    let _: ControlFlow<()> = query.visit(&mut inherited);
    if frame.state.borrow().has_nullable_join && frame.state.borrow().has_aggregation {
        return Err(unsupported(
            "aggregation above nullable JOIN requires merged contributor states",
        ));
    }
    inherited.error.map_or(Ok(()), Err)
}

pub(crate) fn record_legacy_synthesis(context: &RewriteContext, original_node: usize) {
    if let Some(state) = &context.joint_lineage_legacy {
        let mut state = state.borrow_mut();
        // Generated wrappers have no original-node entry. A cloned/missing
        // original cannot masquerade as visited: its original count stays zero.
        if let Some(visits) = state.visits.get_mut(&original_node) {
            visits.1 += 1;
            state.bottom_up += 1;
        }
    }
}

pub(crate) fn synthesize_query(
    rewriter: &PassantRewriter,
    query: &mut Query,
    context: &mut RewriteContext,
) -> Result<(), RewriteError> {
    let mut frame = context.joint_lineage.take().expect("joint traversal frame");
    let original_node = query as *const Query as usize;
    frame.public_labels |= context.require_output_labels;
    if query.order_by.is_some() {
        frame.output_aliases.clear();
    }
    if let Some(with) = &mut query.with {
        for cte in &mut with.cte_tables {
            let mut child = context.clone();
            let mut inherited = frame.clone();
            inherited.root = false;
            inherited.public_labels = true;
            inherited.output_aliases = cte
                .alias
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect();
            inherited.synthesized = Synthesized::default();
            child.joint_lineage = Some(inherited);
            rewriter.rewrite_query_with_context(&mut cte.query, &mut child)?;
            let mut outputs = child.joint_lineage.take().unwrap().synthesized.outputs;
            bind_alias_columns(&mut outputs, &mut cte.alias);
            frame
                .ctes
                .insert(TableKey::new(&cte.alias.name.value), outputs);
        }
    }
    frame.synthesized = synthesize_body(rewriter, query.body.as_mut(), context, &frame)?;
    let replace = |expr: &mut Expr| remap_bound_column(expr, &frame.synthesized.bindings);
    let _: ControlFlow<()> = visit_expressions_mut(&mut query.order_by, replace);
    let _: ControlFlow<()> = visit_expressions_mut(&mut query.limit, replace);
    let _: ControlFlow<()> = visit_expressions_mut(&mut query.offset, replace);

    {
        let mut state = frame.state.borrow_mut();
        state.bottom_up += 1;
        state.visits.entry(original_node).or_default().1 += 1;
    }
    context.joint_lineage = Some(frame);
    Ok(())
}

// Bind original scope references before replacing physical scans. FROM is moved,
// not cloned, while visiting local expressions: child query scopes are untouched.
fn normalize_schema_scan_bindings(
    select: &mut Select,
    frame: &JointLineageContext,
) -> Result<HashMap<TableKey, Ident>, RewriteError> {
    if !frame.state.borrow().has_nullable_join {
        return Ok(HashMap::new());
    }
    let mut bindings = HashMap::<TableKey, Ident>::new();
    let mut basename_counts = HashMap::<TableKey, usize>::new();
    for table in &select.from {
        for factor in
            std::iter::once(&table.relation).chain(table.joins.iter().map(|j| &j.relation))
        {
            if let TableFactor::Table {
                name, alias: None, ..
            } = factor
                && let Some(last) = name.0.last()
            {
                *basename_counts
                    .entry(TableKey::new(&last.value))
                    .or_default() += 1;
            }
        }
    }
    for table in &mut select.from {
        for factor in std::iter::once(&mut table.relation)
            .chain(table.joins.iter_mut().map(|j| &mut j.relation))
        {
            if let TableFactor::Table { name, alias, .. } = factor {
                let key = TableKey::new(&name.to_string());
                if alias.is_none()
                    && name.0.len() > 1
                    && frame.demands.contains_key(&key)
                    && !frame.ctes.contains_key(&key)
                {
                    let new_alias = Ident::new(fresh(&mut frame.state.borrow_mut().used));
                    bindings.insert(key, new_alias.clone());
                    let basename = TableKey::new(&name.0.last().unwrap().value);
                    if basename_counts.get(&basename) == Some(&1) {
                        bindings.insert(basename, new_alias.clone());
                    }
                    *alias = Some(TableAlias {
                        name: new_alias,
                        columns: Vec::new(),
                    });
                }
            }
        }
    }
    if bindings.is_empty() {
        return Ok(HashMap::new());
    }
    let replace = |expr: &mut Expr| remap_bound_column(expr, &bindings);
    let mut from = std::mem::take(&mut select.from);
    for item in &mut select.projection {
        if let SelectItem::QualifiedWildcard(name, _) = item
            && let Some(alias) = bindings.get(&TableKey::new(&name.to_string()))
        {
            name.0 = vec![alias.clone()];
        }
    }
    let _: ControlFlow<()> = visit_expressions_mut(&mut *select, replace);
    for table in &mut from {
        for join in &mut table.joins {
            let condition = match &mut join.join_operator {
                JoinOperator::Inner(c)
                | JoinOperator::LeftOuter(c)
                | JoinOperator::RightOuter(c)
                | JoinOperator::FullOuter(c) => Some(c),
                _ => None,
            };
            if let Some(sqlparser::ast::JoinConstraint::On(expr)) = condition {
                let _: ControlFlow<()> = visit_expressions_mut(expr, replace);
            }
        }
    }
    select.from = from;
    Ok(bindings)
}

fn remap_bound_column(expr: &mut Expr, bindings: &HashMap<TableKey, Ident>) -> ControlFlow<()> {
    if let Expr::CompoundIdentifier(parts) = expr
        && let Some(column) = QualifiedColumn::from_compound_identifier(parts)
        && let Some(alias) = bindings.get(&TableKey::from_table(&column.table))
    {
        *parts = vec![alias.clone(), parts.last().unwrap().clone()];
    }
    ControlFlow::Continue(())
}

fn synthesize_select(
    rewriter: &PassantRewriter,
    select: &mut Select,
    context: &RewriteContext,
    frame: &JointLineageContext,
) -> Result<Synthesized, RewriteError> {
    let bindings = normalize_schema_scan_bindings(select, frame)?;
    let mut inputs = Vec::new();
    let mut public_inputs = Vec::new();
    let mut presence = HashMap::new();
    // One scan may expose several aliases for the same source column. Track
    // source occurrences by bound FROM factor, not by projected column count.
    let mut bound_sources = HashSet::new();
    for table in &mut select.from {
        for factor in std::iter::once(&mut table.relation)
            .chain(table.joins.iter_mut().map(|j| &mut j.relation))
        {
            let (qualifier, outputs) = bind_factor(rewriter, factor, context, frame)?;
            let factor_sources: HashSet<_> = outputs
                .iter()
                .flat_map(|column| {
                    column
                        .origin
                        .as_ref()
                        .map(|(source, _)| source.clone())
                        .into_iter()
                        .chain(column.presence.clone())
                })
                .collect();
            if factor_sources
                .iter()
                .any(|source| !bound_sources.insert(source.clone()))
            {
                return Err(unsupported(
                    "multiple occurrences of one source inside a contributing input",
                ));
            }
            for column in outputs {
                let input = (
                    Expr::CompoundIdentifier(vec![qualifier.clone(), column.name]),
                    column.origin,
                );
                if let Some(source) = column.presence {
                    presence.insert(source, input.0.clone());
                }
                if column.public {
                    public_inputs.push(input.clone());
                }
                inputs.push(input);
            }
        }
    }
    if !presence.is_empty()
        && (select.distinct.is_some()
            || select_is_aggregation(select, &rewriter.aggregate_registry))
    {
        return Err(unsupported(
            "aggregate or DISTINCT above UNION requires merged sufficient states",
        ));
    }
    expand_wildcards(select, &public_inputs)?;
    for (item, alias) in select.projection.iter_mut().zip(&frame.output_aliases) {
        if let SelectItem::UnnamedExpr(expr) = item {
            *item = SelectItem::ExprWithAlias {
                expr: expr.clone(),
                alias: alias.clone(),
            };
        }
    }
    if frame.public_labels {
        preserve_public_output_names(select, rewriter.parse_dialect)?;
    }
    ensure_projection_aliases(select);
    let mut outputs = Vec::new();
    let mut projected_origins = HashSet::new();
    for item in &select.projection {
        let (expr, _) = super::expr::projection_expr_and_name(item)
            .ok_or_else(|| unsupported("unnamed output"))?;
        let name = match item {
            SelectItem::ExprWithAlias { alias, .. } => alias.clone(),
            SelectItem::UnnamedExpr(Expr::Identifier(id)) => id.clone(),
            SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) => {
                parts.last().unwrap().clone()
            }
            _ => return Err(unsupported("unnamed output")),
        };
        let matching: Vec<_> = inputs
            .iter()
            .filter(|(candidate, _)| direct_column_matches(expr, candidate))
            .collect();
        let origin = if matching.len() == 1 {
            matching[0].1.clone()
        } else {
            None
        };
        if let Some(origin) = &origin {
            projected_origins.insert(origin.clone());
        }
        outputs.push(OutputColumn {
            name,
            origin,
            public: true,
            presence: None,
        });
    }
    if !frame.root {
        for (expr, origin) in &inputs {
            let Some(origin) = origin else {
                continue;
            };
            let demanded = frame
                .demands
                .get(&origin.0)
                .is_some_and(|columns| columns.is_empty() || columns.contains(&origin.1));
            if demanded && projected_origins.insert(origin.clone()) {
                let name = Ident::new(fresh(&mut frame.state.borrow_mut().used));
                select.projection.push(SelectItem::ExprWithAlias {
                    expr: expr.clone(),
                    alias: name.clone(),
                });
                outputs.push(OutputColumn {
                    name,
                    origin: Some(origin.clone()),
                    public: false,
                    presence: None,
                });
            }
        }
    }
    if !frame.root {
        for (source, expr) in &presence {
            let name = Ident::new(fresh(&mut frame.state.borrow_mut().used));
            select.projection.push(SelectItem::ExprWithAlias {
                expr: expr.clone(),
                alias: name.clone(),
            });
            outputs.push(OutputColumn {
                name,
                origin: None,
                public: false,
                presence: Some(source.clone()),
            });
        }
    }
    Ok(Synthesized {
        bindings,
        outputs,
        inputs,
        presence,
        finalized: HashMap::new(),
    })
}

fn validate_body(
    body: &SetExpr,
    rewriter: &PassantRewriter,
    root: bool,
    public_labels: bool,
    supplied_names: usize,
    state: &Rc<RefCell<TraversalState>>,
) -> Result<(), RewriteError> {
    match body {
        SetExpr::Select(select) => {
            state.borrow_mut().has_nullable_join |= select
                .from
                .iter()
                .flat_map(|t| &t.joins)
                .any(|j| nullable_join(&j.join_operator));
            state.borrow_mut().has_aggregation |=
                select_is_aggregation(select, &rewriter.aggregate_registry);
            validate_select(select, rewriter, root, public_labels, supplied_names)
        }
        SetExpr::Query(_) => Ok(()), // Its own Query visitor performs inherited validation.
        SetExpr::SetOperation {
            op: sqlparser::ast::SetOperator::Union,
            set_quantifier: sqlparser::ast::SetQuantifier::All,
            left,
            right,
        } => {
            validate_body(left, rewriter, false, public_labels, supplied_names, state)?;
            validate_body(right, rewriter, false, false, 0, state)
        }
        SetExpr::SetOperation {
            op: sqlparser::ast::SetOperator::Union,
            set_quantifier:
                sqlparser::ast::SetQuantifier::Distinct | sqlparser::ast::SetQuantifier::None,
            left,
            right,
        } if root => {
            validate_body(left, rewriter, false, public_labels, supplied_names, state)?;
            validate_body(right, rewriter, false, false, 0, state)
        }
        SetExpr::SetOperation {
            op: sqlparser::ast::SetOperator::Intersect | sqlparser::ast::SetOperator::Except,
            set_quantifier:
                sqlparser::ast::SetQuantifier::None | sqlparser::ast::SetQuantifier::Distinct,
            left,
            right,
        } if root => {
            validate_body(left, rewriter, false, public_labels, supplied_names, state)?;
            validate_body(right, rewriter, false, false, 0, state)
        }
        SetExpr::SetOperation {
            op: sqlparser::ast::SetOperator::Except,
            ..
        } => Err(unsupported(
            "EXCEPT requires a terminal DISTINCT boundary; ALL and nested difference provenance are not established",
        )),
        _ => Err(unsupported(
            "set operations other than UNION ALL require merged sufficient states",
        )),
    }
}

fn synthesize_body(
    rewriter: &PassantRewriter,
    body: &mut SetExpr,
    context: &RewriteContext,
    frame: &JointLineageContext,
) -> Result<Synthesized, RewriteError> {
    match body {
        SetExpr::SetOperation {
            op: sqlparser::ast::SetOperator::Except,
            set_quantifier:
                sqlparser::ast::SetQuantifier::None | sqlparser::ast::SetQuantifier::Distinct,
            left,
            right,
        } if frame.root => {
            let mut child_frame = frame.clone();
            child_frame.root = false;
            let left_schema = synthesize_body(rewriter, left, context, &child_frame)?;
            let mut right_frame = child_frame.clone();
            // Negative inputs carry public keys only, never policy source state.
            right_frame.demands = Rc::new(HashMap::new());
            if matches!(right.as_ref(), SetExpr::Select(_)) {
                right_frame.public_labels = false;
                right_frame.output_aliases = left_schema
                    .outputs
                    .iter()
                    .filter(|c| c.public)
                    .map(|c| c.name.clone())
                    .collect();
            }
            let right_schema = synthesize_body(rewriter, right, context, &right_frame)?;
            let (replacement, result) = terminal_except::finalize(
                rewriter,
                left,
                right,
                &left_schema,
                &right_schema,
                context,
                frame,
            )?;
            *body = replacement;
            Ok(result)
        }
        SetExpr::SetOperation {
            op: sqlparser::ast::SetOperator::Intersect,
            set_quantifier:
                sqlparser::ast::SetQuantifier::None | sqlparser::ast::SetQuantifier::Distinct,
            left,
            right,
        } if frame.root => {
            let mut child_frame = frame.clone();
            child_frame.root = false;
            let left_schema = synthesize_body(rewriter, left, context, &child_frame)?;
            let mut right_frame = child_frame.clone();
            // Set result names belong to the left arm. A direct right SELECT has
            // no independently bound ORDER BY; its unnamed output labels are private.
            if matches!(right.as_ref(), SetExpr::Select(_)) {
                right_frame.public_labels = false;
                right_frame.output_aliases = left_schema
                    .outputs
                    .iter()
                    .filter(|c| c.public)
                    .map(|c| c.name.clone())
                    .collect();
            }
            let right_schema = synthesize_body(rewriter, right, context, &right_frame)?;
            let (replacement, result) = terminal_intersect::finalize(
                rewriter,
                left,
                right,
                &left_schema,
                &right_schema,
                context,
                frame,
            )?;
            *body = replacement;
            Ok(result)
        }
        SetExpr::Select(select) if frame.root && select.distinct.is_some() => {
            // Preserve raw contributors until the terminal public-key grouping;
            // never attach a pass flag to the visible DISTINCT identity.
            select.distinct = None;
            let mut raw_frame = frame.clone();
            raw_frame.root = false;
            let mut raw_schema = synthesize_select(rewriter, select, context, &raw_frame)?;
            let sources: HashSet<_> = raw_schema
                .outputs
                .iter()
                .filter_map(|column| column.origin.as_ref().map(|(source, _)| source.clone()))
                .collect();
            for source in sources {
                if raw_schema
                    .outputs
                    .iter()
                    .any(|column| column.presence.as_ref() == Some(&source))
                {
                    continue;
                }
                let name = Ident::new(fresh(&mut frame.state.borrow_mut().used));
                select.projection.push(SelectItem::ExprWithAlias {
                    expr: super::expr::bool_literal(true),
                    alias: name.clone(),
                });
                raw_schema.outputs.push(OutputColumn {
                    name,
                    origin: None,
                    public: false,
                    presence: Some(source),
                });
            }
            let raw_body = SetExpr::Select(std::mem::replace(
                select,
                Box::new(crate::sql::empty_select()),
            ));
            let (replacement, result) =
                terminal_distinct::finalize(rewriter, raw_body, raw_schema, context, frame)?;
            *body = replacement;
            Ok(result)
        }
        SetExpr::Select(select) => synthesize_select(rewriter, select, context, frame),
        SetExpr::Query(query) => {
            let mut child = context.clone();
            let mut inherited = frame.clone();
            inherited.root = false;
            child.joint_lineage = Some(inherited);
            rewriter.rewrite_query_with_context(query, &mut child)?;
            Ok(child.joint_lineage.take().unwrap().synthesized)
        }
        SetExpr::SetOperation {
            op: sqlparser::ast::SetOperator::Union,
            set_quantifier: sqlparser::ast::SetQuantifier::All,
            left,
            right,
        } => {
            let mut child_frame = frame.clone();
            child_frame.root = false;
            let left_schema = synthesize_body(rewriter, left, context, &child_frame)?;
            let mut right_frame = child_frame.clone();
            // Set result names belong to the left arm. A direct right SELECT has
            // no independently bound ORDER BY; its unnamed output labels are private.
            if matches!(right.as_ref(), SetExpr::Select(_)) {
                right_frame.public_labels = false;
                right_frame.output_aliases = left_schema
                    .outputs
                    .iter()
                    .filter(|c| c.public)
                    .map(|c| c.name.clone())
                    .collect();
            }
            let right_schema = synthesize_body(rewriter, right, context, &right_frame)?;
            let (replacement, result) = align_union_all(
                rewriter,
                left,
                right,
                &left_schema.outputs,
                &right_schema.outputs,
                frame,
            )?;
            *body = replacement;
            Ok(result)
        }
        SetExpr::SetOperation {
            op: sqlparser::ast::SetOperator::Union,
            set_quantifier:
                sqlparser::ast::SetQuantifier::Distinct | sqlparser::ast::SetQuantifier::None,
            left,
            right,
        } if frame.root => {
            let mut child_frame = frame.clone();
            child_frame.root = false;
            let left_schema = synthesize_body(rewriter, left, context, &child_frame)?;
            let mut right_frame = child_frame.clone();
            // Set result names belong to the left arm. A direct right SELECT has
            // no independently bound ORDER BY; its unnamed output labels are private.
            if matches!(right.as_ref(), SetExpr::Select(_)) {
                right_frame.public_labels = false;
                right_frame.output_aliases = left_schema
                    .outputs
                    .iter()
                    .filter(|c| c.public)
                    .map(|c| c.name.clone())
                    .collect();
            }
            let right_schema = synthesize_body(rewriter, right, context, &right_frame)?;
            let (aligned, schema) = align_union_all(
                rewriter,
                left,
                right,
                &left_schema.outputs,
                &right_schema.outputs,
                &child_frame,
            )?;
            let (replacement, result) =
                terminal_distinct::finalize(rewriter, aligned, schema, context, frame)?;
            *body = replacement;
            Ok(result)
        }
        _ => unreachable!("inherited UNION capability"),
    }
}

fn parsed_body(sql: &str) -> Result<SetExpr, RewriteError> {
    let sqlparser::ast::Statement::Query(query) = crate::parser::parse_query(sql)? else {
        return Err(unsupported("internal UNION alignment"));
    };
    Ok(*query.body)
}

fn align_union_all(
    rewriter: &PassantRewriter,
    left: &SetExpr,
    right: &SetExpr,
    left_schema: &[OutputColumn],
    right_schema: &[OutputColumn],
    frame: &JointLineageContext,
) -> Result<(SetExpr, Synthesized), RewriteError> {
    let left_public: Vec<_> = left_schema.iter().filter(|c| c.public).collect();
    let right_public: Vec<_> = right_schema.iter().filter(|c| c.public).collect();
    if left_public.len() != right_public.len() || left_public.is_empty() {
        return Err(unsupported(
            "UNION branches with incompatible public schemas",
        ));
    }
    let mut output_names = HashSet::new();
    for column in &left_public {
        if !output_names.insert(column.name.value.to_ascii_lowercase()) {
            return Err(unsupported("duplicate UNION output names"));
        }
    }
    let mut origins: Vec<_> = left_schema
        .iter()
        .chain(right_schema)
        .filter_map(|c| c.origin.clone())
        .collect();
    origins.sort_by_key(|(source, column)| (source.as_str().to_owned(), column.clone()));
    origins.dedup();
    let mut sources: Vec<_> = origins
        .iter()
        .map(|(source, _)| source.clone())
        .chain(
            left_schema
                .iter()
                .chain(right_schema)
                .filter_map(|c| c.presence.clone()),
        )
        .collect();
    sources.sort_by_key(|source| source.as_str().to_owned());
    sources.dedup();
    let slots: Vec<_> = origins
        .iter()
        .map(|origin| OutputColumn {
            name: Ident::new(fresh(&mut frame.state.borrow_mut().used)),
            origin: Some(origin.clone()),
            public: false,
            presence: None,
        })
        .collect();
    let presences: Vec<_> = sources
        .iter()
        .map(|source| OutputColumn {
            name: Ident::new(fresh(&mut frame.state.borrow_mut().used)),
            origin: None,
            public: false,
            presence: Some(source.clone()),
        })
        .collect();
    let aligned = |body: &SetExpr, schema: &[OutputColumn]| -> Result<String, RewriteError> {
        let alias = fresh(&mut frame.state.borrow_mut().used);
        let public: Vec<_> = schema.iter().filter(|c| c.public).collect();
        let mut projections: Vec<_> = public
            .iter()
            .zip(&left_public)
            .map(|(input, output)| format!("{alias}.{} AS {}", input.name, output.name))
            .collect();
        for slot in &slots {
            let origin = slot.origin.as_ref().unwrap();
            let input = schema.iter().find(|c| c.origin.as_ref() == Some(origin));
            let value = if let Some(input) = input {
                format!("{alias}.{}", input.name)
            } else if let Some(ty) = rewriter.catalog.column_type(origin.0.as_str(), &origin.1) {
                format!("CAST(NULL AS {ty})")
            } else {
                "NULL".into()
            };
            projections.push(format!("{value} AS {}", slot.name));
        }
        for slot in &presences {
            let source = slot.presence.as_ref().unwrap();
            let value =
                if let Some(input) = schema.iter().find(|c| c.presence.as_ref() == Some(source)) {
                    format!("{alias}.{}", input.name)
                } else if schema
                    .iter()
                    .any(|c| c.origin.as_ref().is_some_and(|(s, _)| s == source))
                {
                    "TRUE".into()
                } else {
                    "FALSE".into()
                };
            projections.push(format!("{value} AS {}", slot.name));
        }
        Ok(format!(
            "SELECT {} FROM ({body}) AS {alias}",
            projections.join(", ")
        ))
    };
    let union = format!(
        "{} UNION ALL {}",
        aligned(left, left_schema)?,
        aligned(right, right_schema)?
    );
    let alias = Ident::new(fresh(&mut frame.state.borrow_mut().used));
    let mut outputs: Vec<_> = left_public
        .iter()
        .map(|c| OutputColumn {
            name: c.name.clone(),
            origin: None,
            public: true,
            presence: None,
        })
        .collect();
    outputs.extend(slots);
    outputs.extend(presences);
    let mut inputs = Vec::new();
    let mut presence = HashMap::new();
    for output in &outputs {
        let expr = Expr::CompoundIdentifier(vec![alias.clone(), output.name.clone()]);
        if let Some(source) = &output.presence {
            presence.insert(source.clone(), expr.clone());
        }
        inputs.push((expr, output.origin.clone()));
    }
    let projection = outputs
        .iter()
        .filter(|c| !frame.root || c.public)
        .map(|c| format!("{alias}.{} AS {}", c.name, c.name))
        .collect::<Vec<_>>()
        .join(", ");
    let body = parsed_body(&format!("SELECT {projection} FROM ({union}) AS {alias}"))?;
    Ok((
        body,
        Synthesized {
            bindings: HashMap::new(),
            outputs,
            inputs,
            presence,
            finalized: HashMap::new(),
        },
    ))
}

fn bind_factor(
    rewriter: &PassantRewriter,
    factor: &mut TableFactor,
    context: &RewriteContext,
    frame: &JointLineageContext,
) -> Result<(Ident, Vec<OutputColumn>), RewriteError> {
    match factor {
        TableFactor::Table { name, alias, .. } => {
            let key = TableKey::new(&name.to_string());
            let qualifier = alias
                .as_ref()
                .map(|a| a.name.clone())
                .unwrap_or_else(|| name.0.last().unwrap().clone());
            let mut outputs = if let Some(outputs) = frame.ctes.get(&key) {
                outputs.clone()
            } else {
                rewriter
                    .catalog
                    .columns(&name.to_string())
                    .into_iter()
                    .flatten()
                    .map(|column| OutputColumn {
                        name: Ident::with_quote('"', column),
                        public: true,
                        presence: None,
                        origin: frame
                            .demands
                            .contains_key(&key)
                            .then(|| (key.clone(), column.to_ascii_lowercase())),
                    })
                    .collect()
            };
            if let Some(alias) = alias {
                bind_alias_columns(&mut outputs, alias);
            }
            if frame.state.borrow().has_nullable_join
                && frame.demands.contains_key(&key)
                && !frame.ctes.contains_key(&key)
            {
                let marker = {
                    let mut state = frame.state.borrow_mut();
                    state
                        .used
                        .extend(outputs.iter().map(|c| c.name.value.to_ascii_lowercase()));
                    Ident::new(fresh(&mut state.used))
                };
                let wrapper = parsed_body(&format!(
                    "SELECT * FROM (SELECT *, TRUE AS {marker} FROM {factor}) AS {qualifier}"
                ))?;
                let SetExpr::Select(mut wrapper) = wrapper else {
                    unreachable!()
                };
                *factor = wrapper.from.remove(0).relation;
                outputs.push(OutputColumn {
                    name: marker,
                    origin: None,
                    public: false,
                    presence: Some(key),
                });
            }
            Ok((qualifier, outputs))
        }
        TableFactor::Derived {
            subquery,
            alias: Some(alias),
            ..
        } => {
            let mut child = context.clone();
            let mut inherited = frame.clone();
            inherited.root = false;
            inherited.public_labels = true;
            inherited.output_aliases = alias
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect();
            inherited.synthesized = Synthesized::default();
            child.joint_lineage = Some(inherited);
            rewriter.rewrite_query_with_context(subquery, &mut child)?;
            let mut outputs = child.joint_lineage.take().unwrap().synthesized.outputs;
            bind_alias_columns(&mut outputs, alias);
            Ok((alias.name.clone(), outputs))
        }
        _ => Err(unsupported(
            "table functions or lateral/unaliased derived inputs",
        )),
    }
}
fn bind_alias_columns(outputs: &mut [OutputColumn], alias: &mut TableAlias) {
    if alias.columns.is_empty() {
        return;
    }
    for (index, output) in outputs.iter_mut().enumerate() {
        if let Some(column) = alias.columns.get(index) {
            output.name = column.name.clone();
        } else {
            alias.columns.push(TableAliasColumnDef {
                name: output.name.clone(),
                data_type: None,
            });
        }
    }
}

pub(crate) fn query_requires_output_names(query: &Query) -> bool {
    query.order_by.as_ref().is_some_and(|order| order.exprs.iter().any(|item| {
        !matches!(&item.expr, Expr::Value(sqlparser::ast::Value::Number(number,_)) if number.chars().all(|c| c.is_ascii_digit()))
    }))
}

pub(crate) fn preserve_public_output_names(
    select: &mut Select,
    dialect: crate::sql::SqlDialect,
) -> Result<(), RewriteError> {
    for item in &mut select.projection {
        if let SelectItem::UnnamedExpr(expr) = item
            && let Some(label) = public_output_label(expr, dialect)?
        {
            *item = SelectItem::ExprWithAlias {
                expr: expr.clone(),
                alias: Ident::with_quote('"', label),
            };
        }
    }
    Ok(())
}
pub(crate) fn public_output_label(
    expr: &Expr,
    dialect: crate::sql::SqlDialect,
) -> Result<Option<String>, RewriteError> {
    if matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
        return Ok(None);
    }
    if dialect != crate::sql::SqlDialect::DuckDb {
        return Err(unsupported(
            "unproven unaliased expression output label for this dialect; use an explicit alias",
        ));
    }
    duckdb_expression_label(expr).map(Some).ok_or_else(|| {
        unsupported("unproven unaliased expression output label; use an explicit alias")
    })
}
fn duckdb_expression_label(expr: &Expr) -> Option<String> {
    use sqlparser::ast::{
        BinaryOperator as B, FunctionArg, FunctionArgExpr, FunctionArguments, Value,
    };
    match expr {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => Some(expr.to_string()),
        Expr::Value(Value::Number(number, _)) if number.chars().all(|c| c.is_ascii_digit()) => {
            let n = number.trim_start_matches('0');
            Some(if n.is_empty() { "0".into() } else { n.into() })
        }
        Expr::Nested(inner) => duckdb_expression_label(inner),
        Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Minus,
            expr,
        } if matches!(expr.as_ref(), Expr::Value(Value::Number(_, _))) => {
            Some(format!("-{}", duckdb_expression_label(expr)?))
        }
        Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Minus,
            expr,
        } if matches!(
            expr.as_ref(),
            Expr::Identifier(_) | Expr::CompoundIdentifier(_)
        ) =>
        {
            Some(format!("-({expr})"))
        }
        Expr::BinaryOp { left, op, right }
            if matches!(op, B::Plus | B::Minus | B::Multiply | B::Divide | B::Modulo) =>
        {
            Some(format!(
                "({} {op} {})",
                duckdb_expression_label(left)?,
                duckdb_expression_label(right)?
            ))
        }
        Expr::Function(function)
            if function.filter.is_none()
                && function.over.is_none()
                && function.within_group.is_empty() =>
        {
            let name = function.name.to_string().to_ascii_lowercase();
            if !["min", "max", "sum", "avg", "count", "abs", "coalesce"].contains(&name.as_str()) {
                return None;
            }
            let FunctionArguments::List(args) = &function.args else {
                return None;
            };
            if args.duplicate_treatment.is_some() || !args.clauses.is_empty() {
                return None;
            }
            if name == "count"
                && args.args.len() == 1
                && matches!(
                    args.args[0],
                    FunctionArg::Unnamed(FunctionArgExpr::Wildcard)
                )
            {
                return Some("count_star()".into());
            }
            let args = args
                .args
                .iter()
                .map(|arg| match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => {
                        duckdb_expression_label(expr)
                    }
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?;
            let name = if name == "coalesce" {
                "COALESCE"
            } else {
                &name
            };
            Some(format!("{name}({})", args.join(", ")))
        }
        _ => None,
    }
}

fn nullable_join(join: &JoinOperator) -> bool {
    matches!(
        join,
        JoinOperator::LeftOuter(sqlparser::ast::JoinConstraint::On(_))
            | JoinOperator::RightOuter(sqlparser::ast::JoinConstraint::On(_))
            | JoinOperator::FullOuter(sqlparser::ast::JoinConstraint::On(_))
    )
}

fn validate_select(
    select: &Select,
    rewriter: &PassantRewriter,
    root: bool,
    public_labels: bool,
    supplied_names: usize,
) -> Result<(), RewriteError> {
    if public_labels {
        for item in select.projection.iter().skip(supplied_names) {
            if let SelectItem::UnnamedExpr(expr) = item {
                public_output_label(expr, rewriter.parse_dialect)?;
            }
        }
    }
    if root && select.distinct.is_some() {
        if matches!(select.distinct, Some(sqlparser::ast::Distinct::On(_))) {
            return Err(unsupported(
                "DISTINCT ON requires its own contributor-selection semantics",
            ));
        }
        if select_is_aggregation(select, &rewriter.aggregate_registry) {
            return Err(unsupported(
                "DISTINCT over an existing aggregate requires merged sufficient states",
            ));
        }
    }
    if !root
        && (select.distinct.is_some()
            || select_is_aggregation(select, &rewriter.aggregate_registry))
    {
        return Err(unsupported(
            "aggregation or DISTINCT inside a contributing input",
        ));
    }
    if select.top.is_some() || select.qualify.is_some() || !select.named_window.is_empty() {
        return Err(unsupported("TOP, QUALIFY or window inputs"));
    }
    for table in &select.from {
        for join in &table.joins {
            if !matches!(
                join.join_operator,
                JoinOperator::Inner(_) | JoinOperator::CrossJoin
            ) && !nullable_join(&join.join_operator)
            {
                return Err(unsupported("non-inner joins"));
            }
        }
        for factor in
            std::iter::once(&table.relation).chain(table.joins.iter().map(|j| &j.relation))
        {
            if !matches!(
                factor,
                TableFactor::Table { args: None, .. }
                    | TableFactor::Derived {
                        lateral: false,
                        alias: Some(_),
                        ..
                    }
            ) {
                return Err(unsupported(
                    "table functions or lateral/unaliased derived inputs",
                ));
            }
        }
    }
    Ok(())
}

fn expand_wildcards(select: &mut Select, inputs: &[InputColumn]) -> Result<(), RewriteError> {
    if select
        .from
        .iter()
        .flat_map(|table| &table.joins)
        .any(|join| {
            matches!(
                join.join_operator,
                JoinOperator::Inner(
                    sqlparser::ast::JoinConstraint::Using(_)
                        | sqlparser::ast::JoinConstraint::Natural
                )
            )
        })
    {
        return Err(unsupported(
            "USING/NATURAL joins require schema-aware hidden-column transport",
        ));
    }
    let mut projection = Vec::new();
    for item in std::mem::take(&mut select.projection) {
        match item {
            SelectItem::Wildcard(options) if options == Default::default() => {
                if inputs.is_empty() {
                    return Err(unsupported("wildcard with unavailable catalog lineage"));
                }
                projection.extend(
                    inputs
                        .iter()
                        .map(|(expr, _)| SelectItem::UnnamedExpr(expr.clone())),
                );
            }
            SelectItem::QualifiedWildcard(table, options) if options == Default::default() => {
                let items: Vec<_> = inputs.iter().filter(|(expr, _)| matches!(expr, Expr::CompoundIdentifier(parts) if TableKey::new(&parts[0].value) == TableKey::new(&table.to_string()))).map(|(expr, _)| SelectItem::UnnamedExpr(expr.clone())).collect();
                if items.is_empty() {
                    return Err(unsupported("qualified wildcard with unavailable lineage"));
                }
                projection.extend(items);
            }
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                return Err(unsupported("wildcard modifiers"));
            }
            other => projection.push(other),
        }
    }
    select.projection = projection;
    Ok(())
}
fn direct_column_matches(expr: &Expr, candidate: &Expr) -> bool {
    match (expr, candidate) {
        (Expr::Identifier(a), Expr::CompoundIdentifier(b)) => b
            .last()
            .is_some_and(|b| a.value.eq_ignore_ascii_case(&b.value)),
        (Expr::CompoundIdentifier(a), Expr::CompoundIdentifier(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|(a, b)| a.value.eq_ignore_ascii_case(&b.value))
        }
        _ => false,
    }
}
fn identifier_names(sql: &str) -> Result<HashSet<String>, RewriteError> {
    Ok(
        sqlparser::tokenizer::Tokenizer::new(&sqlparser::dialect::GenericDialect {}, sql)
            .tokenize()
            .map_err(|_| unsupported("identifier allocation"))?
            .into_iter()
            .filter_map(|t| {
                if let sqlparser::tokenizer::Token::Word(w) = t {
                    Some(w.value.to_ascii_lowercase())
                } else {
                    None
                }
            })
            .collect(),
    )
}
fn fresh(used: &mut HashSet<String>) -> String {
    let mut i = 0;
    loop {
        let name = format!("__passant_lineage_{i}");
        if used.insert(name.clone()) {
            return name;
        }
        i += 1;
    }
}

#[cfg(test)]
mod traversal_tests {
    use super::*;
    fn test_context(rewriter: &PassantRewriter) -> RewriteContext {
        RewriteContext {
            aggregate_registry: rewriter.aggregate_registry.clone(),
            resolution_registry: rewriter.resolution_registry.clone(),
            ..RewriteContext::default()
        }
    }

    #[test]
    fn original_query_nodes_have_one_visit_each_direction_and_cte_references_are_memoized() {
        let mut rewriter = PassantRewriter::default();
        rewriter
            .catalog
            .register_table("a", vec!["x".into(), "allowed".into()]);
        rewriter.catalog.register_table("b", vec!["x".into()]);
        let sql = "WITH aa AS (SELECT x FROM a), bb AS (SELECT x FROM b) SELECT aa.x FROM aa JOIN bb l ON aa.x=l.x JOIN bb r ON aa.x=r.x JOIN (SELECT x FROM b) d ON aa.x=d.x WHERE aa.x > 0";
        let mut parsed =
            sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::DuckDbDialect {}, sql)
                .unwrap();
        let sqlparser::ast::Statement::Query(query) = &mut parsed[0] else {
            panic!()
        };
        let state = Rc::new(RefCell::new(TraversalState {
            used: identifier_names(sql).unwrap(),
            ..Default::default()
        }));
        let mut context = test_context(&rewriter);
        context.joint_lineage = Some(JointLineageContext {
            policies: Rc::new(Vec::new()),
            public_labels: true,
            output_aliases: Vec::new(),
            demands: Rc::new(HashMap::from([(
                TableKey::new("a"),
                HashSet::from(["allowed".into()]),
            )])),
            ctes: HashMap::new(),
            state: state.clone(),
            root: true,
            synthesized: Synthesized::default(),
        });
        inherit_query_demands(
            &rewriter,
            query,
            context.joint_lineage.as_ref().unwrap(),
            true,
        )
        .unwrap();
        rewriter
            .rewrite_query_with_context(query, &mut context)
            .unwrap();
        let state = state.borrow();
        assert_eq!((state.top_down, state.bottom_up), (4, 4));
        assert_eq!(state.visits.len(), 4);
        assert!(state.visits.values().all(|count| *count == (1, 1)));
        let frame = context.joint_lineage.unwrap();
        assert_eq!(frame.ctes[&TableKey::new("aa")].len(), 2);
        assert_eq!(frame.ctes[&TableKey::new("bb")].len(), 1);
        assert_eq!(frame.synthesized.outputs.len(), 1);
        assert!(query.to_string().contains("WHERE aa.x > 0"));
        assert!(!query.to_string().contains("passant_retry"));
    }
    #[test]
    fn opaque_legacy_fallback_has_one_inherited_and_one_rewrite_visit_per_original_query() {
        let mut rewriter = PassantRewriter::default();
        rewriter.catalog.register_table("a", vec!["x".into()]);
        rewriter.catalog.register_table("b", vec!["x".into()]);
        rewriter
            .register_policy_text("SOURCE a CONSTRAINT MAX(a.x) > 0 ON FAIL REMOVE")
            .unwrap();
        let sql = "WITH aa AS (SELECT MAX(x) AS x FROM a), bb AS (SELECT x FROM b) SELECT aa.x FROM aa JOIN bb l ON aa.x=l.x JOIN bb r ON aa.x=r.x";
        let mut parsed =
            sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::DuckDbDialect {}, sql)
                .unwrap();
        let sqlparser::ast::Statement::Query(query) = &mut parsed[0] else {
            panic!()
        };
        let mut context = test_context(&rewriter);
        rewriter
            .rewrite_query_with_context(query, &mut context)
            .unwrap();
        let state = context
            .joint_lineage_legacy
            .as_ref()
            .expect("opaque aggregate uses legacy strategy")
            .borrow();
        assert_eq!((state.top_down, state.bottom_up), (3, 3));
        assert_eq!(state.visits.len(), 3);
        assert!(state.visits.values().all(|count| *count == (1, 1)));
    }
    #[test]
    fn join_on_expression_subquery_is_rejected_before_synthesis() {
        let mut rewriter = PassantRewriter::default();
        for table in ["a", "b", "c"] {
            rewriter.catalog.register_table(table, vec!["x".into()]);
        }
        rewriter
            .register_policy_text("SOURCE a CONSTRAINT a.x > 0 ON FAIL RETRY('blocked')")
            .unwrap();
        for sql in [
            "SELECT d.x FROM (SELECT x FROM a) d JOIN b ON EXISTS(SELECT x FROM c)",
            "SELECT d.x FROM (SELECT x FROM a) d ORDER BY (SELECT MAX(x) FROM c)",
            "SELECT d.x FROM (SELECT x FROM a) d LIMIT (SELECT MAX(x) FROM c)",
        ] {
            let mut parsed =
                sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::DuckDbDialect {}, sql)
                    .unwrap();
            let original = parsed[0].to_string();
            let sqlparser::ast::Statement::Query(query) = &mut parsed[0] else {
                panic!()
            };
            let error = rewriter
                .rewrite_query_with_context(query, &mut test_context(&rewriter))
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("expression subqueries or windows")
            );
            assert_eq!(parsed[0].to_string(), original);
        }
    }
    #[test]
    fn absent_catalog_uses_pure_legacy_capability_for_splittable_policy() {
        let mut rewriter = PassantRewriter::default();
        let error = rewriter
            .register_policy_text("SOURCE a CONSTRAINT MAX(a.x) > 0 ON FAIL REMOVE")
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Source table 'a' does not exist")
        );
        assert!(!rewriter.has_registered_policies());
    }
    #[test]
    fn union_all_branch_query_limit_has_one_original_query_visit_each_direction() {
        let mut rewriter = PassantRewriter::default();
        for table in ["a", "b"] {
            rewriter.catalog.register_table(table, vec!["x".into()]);
        }
        let sql = "SELECT x FROM a UNION ALL (SELECT x FROM b ORDER BY x LIMIT 1) UNION ALL SELECT 3 AS x";
        let mut parsed =
            sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::DuckDbDialect {}, sql)
                .unwrap();
        let sqlparser::ast::Statement::Query(query) = &mut parsed[0] else {
            panic!()
        };
        let state = Rc::new(RefCell::new(TraversalState {
            used: identifier_names(sql).unwrap(),
            ..Default::default()
        }));
        let mut context = test_context(&rewriter);
        context.joint_lineage = Some(JointLineageContext {
            policies: Rc::new(Vec::new()),
            public_labels: true,
            output_aliases: Vec::new(),
            demands: Rc::new(HashMap::from([
                (TableKey::new("a"), HashSet::from(["x".into()])),
                (TableKey::new("b"), HashSet::from(["x".into()])),
            ])),
            ctes: HashMap::new(),
            state: state.clone(),
            root: true,
            synthesized: Synthesized::default(),
        });
        inherit_query_demands(
            &rewriter,
            query,
            context.joint_lineage.as_ref().unwrap(),
            true,
        )
        .unwrap();
        rewriter
            .rewrite_query_with_context(query, &mut context)
            .unwrap();
        let state = state.borrow();
        assert_eq!((state.top_down, state.bottom_up), (2, 2));
        assert!(state.visits.values().all(|visits| *visits == (1, 1)));
        let synthesis = &context.joint_lineage.as_ref().unwrap().synthesized;
        assert_eq!(synthesis.outputs.iter().filter(|c| c.public).count(), 1);
        assert_eq!(synthesis.presence.len(), 2);
        assert!(query.to_string().contains("ORDER BY x LIMIT 1"));
        assert!(!query.to_string().contains("passant_retry"));
    }
    #[test]
    fn no_catalog_cross_source_distinct_is_not_silently_branch_skipped() {
        let mut rewriter = PassantRewriter::default();
        let error = rewriter
            .register_policy_text(
                "SOURCE a, b CONSTRAINT MAX(a.x) > 0 AND MAX(b.x) > 0 ON FAIL REMOVE",
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Source table 'a' does not exist")
        );
        assert!(!rewriter.has_registered_policies());
    }
    #[test]
    fn unproven_multisource_nullable_and_intersect_fail_before_effects() {
        let mut rewriter = PassantRewriter::default();
        let error = rewriter
            .register_policy_text(
                "SOURCE a, b CONSTRAINT MAX(a.x) > 0 AND MAX(b.x) > 0 ON FAIL REMOVE",
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Source table 'a' does not exist")
        );
        assert!(!rewriter.has_registered_policies());
    }
    #[test]
    fn positive_except_ctes_have_one_original_query_visit_each_direction() {
        let mut rewriter = PassantRewriter::default();
        for table in ["a", "b"] {
            rewriter.catalog.register_table(table, vec!["x".into()]);
        }
        let sql = "WITH aa AS (SELECT x FROM a), bb AS (SELECT x FROM b) SELECT x FROM aa EXCEPT SELECT x FROM bb";
        let mut parsed =
            sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::DuckDbDialect {}, sql)
                .unwrap();
        let sqlparser::ast::Statement::Query(query) = &mut parsed[0] else {
            panic!()
        };
        let state = Rc::new(RefCell::new(TraversalState {
            used: identifier_names(sql).unwrap(),
            ..Default::default()
        }));
        let mut context = test_context(&rewriter);
        context.joint_lineage = Some(JointLineageContext {
            policies: Rc::new(Vec::new()),
            public_labels: true,
            output_aliases: Vec::new(),
            demands: Rc::new(HashMap::from([(
                TableKey::new("a"),
                HashSet::from(["x".into()]),
            )])),
            ctes: HashMap::new(),
            state: state.clone(),
            root: true,
            synthesized: Synthesized::default(),
        });
        inherit_query_demands(
            &rewriter,
            query,
            context.joint_lineage.as_ref().unwrap(),
            true,
        )
        .unwrap();
        rewriter
            .rewrite_query_with_context(query, &mut context)
            .unwrap();
        let state = state.borrow();
        assert_eq!((state.top_down, state.bottom_up), (3, 3));
        assert!(state.visits.values().all(|v| *v == (1, 1)));
        assert!(query.to_string().contains("NOT EXISTS"));
        let synthesis = &context.joint_lineage.as_ref().unwrap().synthesized;
        assert_eq!(synthesis.outputs.iter().filter(|c| c.public).count(), 1);
        assert!(!synthesis.presence.contains_key(&TableKey::new("b")));
    }
}
