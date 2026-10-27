//! Terminal DISTINCT finalization over raw contributors. These predicate slots
//! are final results, NOT mergeable semiring state and cannot cross another
//! duplicate-elimination or aggregation boundary.
use super::*;
use sqlparser::ast::{VisitMut, VisitorMut};

pub(super) fn finalize(
    rewriter: &PassantRewriter,
    aligned: SetExpr,
    raw_schema: Synthesized,
    context: &RewriteContext,
    frame: &JointLineageContext,
) -> Result<(SetExpr, Synthesized), RewriteError> {
    let input_alias = Ident::new(fresh(&mut frame.state.borrow_mut().used));
    let output_alias = Ident::new(fresh(&mut frame.state.borrow_mut().used));
    let public: Vec<_> = raw_schema.outputs.iter().filter(|c| c.public).collect();
    let qualify = |name: &Ident| Expr::CompoundIdentifier(vec![input_alias.clone(), name.clone()]);
    let raw: HashMap<_, _> = raw_schema
        .outputs
        .iter()
        .filter_map(|c| c.origin.clone().map(|key| (key, qualify(&c.name))))
        .collect();
    let present: HashMap<_, _> = raw_schema
        .outputs
        .iter()
        .filter_map(|c| c.presence.clone().map(|key| (key, qualify(&c.name))))
        .collect();
    let mut output_columns: HashMap<String, Expr> = public
        .iter()
        .map(|c| (c.name.value.to_ascii_lowercase(), qualify(&c.name)))
        .collect();
    for (target, column) in context.sink_output_columns.iter().zip(&public) {
        output_columns.insert(target.value.to_ascii_lowercase(), qualify(&column.name));
    }
    let mut projection: Vec<_> = public
        .iter()
        .map(|c| format!("{} AS {}", qualify(&c.name), c.name))
        .collect();
    let groups = public
        .iter()
        .map(|c| qualify(&c.name).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let mut outputs: Vec<_> = public
        .iter()
        .map(|c| OutputColumn {
            name: c.name.clone(),
            origin: None,
            public: true,
            presence: None,
        })
        .collect();
    let mut presence = HashMap::new();
    for (source, input) in &present {
        let name = Ident::new(fresh(&mut frame.state.borrow_mut().used));
        projection.push(format!(
            "MAX(CASE WHEN ({input}) IS TRUE THEN 1 ELSE 0 END) > 0 AS {name}"
        ));
        outputs.push(OutputColumn {
            name: name.clone(),
            origin: None,
            public: false,
            presence: Some(source.clone()),
        });
        presence.insert(
            source.clone(),
            Expr::CompoundIdentifier(vec![output_alias.clone(), name]),
        );
    }
    let mut finalized = HashMap::new();
    for index in frame.policies.iter() {
        let policy = rewriter.store.policy(*index).unwrap();
        let PolicyIr::Pgn {
            sources,
            source_aliases,
            constraint,
            sink,
            sink_alias,
            ..
        } = policy;
        let mut expr = super::super::columns::replace_source_alias_qualifiers(
            crate::sql::parse_policy_expr_duckdb(constraint)?,
            source_aliases,
        );
        if source_column(&expr, sources, &raw).is_some() {
            return Err(unsupported(
                "terminal DISTINCT bare Boolean source attribute has no established scalar desugaring",
            ));
        }
        let mut transform = FinalizeExpr {
            rewriter,
            sources,
            sink: sink.as_deref(),
            sink_alias: sink_alias.as_deref(),
            raw: &raw,
            present: &present,
            outputs: &output_columns,
            aggregates: Vec::new(),
            guards: Vec::new(),
            guarded: HashSet::new(),
        };
        if let ControlFlow::Break(error) = expr.visit(&mut transform) {
            return Err(error);
        }
        let mut terms = transform.guards;
        terms.push(format!("({expr})"));
        let name = Ident::new(fresh(&mut frame.state.borrow_mut().used));
        projection.push(format!("({}) AS {name}", terms.join(" AND ")));
        outputs.push(OutputColumn {
            name: name.clone(),
            origin: None,
            public: false,
            presence: None,
        });
        finalized.insert(
            *index,
            Expr::CompoundIdentifier(vec![output_alias.clone(), name]),
        );
    }
    let grouped = format!(
        "SELECT {} FROM ({aligned}) AS {input_alias} GROUP BY {groups}",
        projection.join(", ")
    );
    let visible = public
        .iter()
        .map(|c| format!("{output_alias}.{} AS {}", c.name, c.name))
        .collect::<Vec<_>>()
        .join(", ");
    let body = parsed_body(&format!(
        "SELECT {visible} FROM ({grouped}) AS {output_alias}"
    ))?;
    let inputs = outputs
        .iter()
        .map(|c| {
            (
                Expr::CompoundIdentifier(vec![output_alias.clone(), c.name.clone()]),
                None,
            )
        })
        .collect();
    Ok((
        body,
        Synthesized {
            bindings: raw_schema.bindings,
            outputs,
            inputs,
            presence,
            finalized,
        },
    ))
}

type Origin = (TableKey, String);
fn source_column(expr: &Expr, sources: &[String], raw: &HashMap<Origin, Expr>) -> Option<Origin> {
    let key = match expr {
        Expr::CompoundIdentifier(parts) => {
            let column = QualifiedColumn::from_compound_identifier(parts)?;
            (TableKey::from_table(&column.table), column.column.key())
        }
        Expr::Identifier(id) if sources.len() == 1 => {
            (TableKey::new(&sources[0]), id.value.to_ascii_lowercase())
        }
        _ => return None,
    };
    raw.contains_key(&key).then_some(key)
}
struct FinalizeExpr<'a> {
    rewriter: &'a PassantRewriter,
    sources: &'a [String],
    sink: Option<&'a str>,
    sink_alias: Option<&'a str>,
    raw: &'a HashMap<Origin, Expr>,
    present: &'a HashMap<TableKey, Expr>,
    outputs: &'a HashMap<String, Expr>,
    aggregates: Vec<(usize, Option<TableKey>)>,
    guards: Vec<String>,
    guarded: HashSet<Origin>,
}
impl VisitorMut for FinalizeExpr<'_> {
    type Break = RewriteError;
    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        if let Expr::Function(function) = expr
            && self.rewriter.aggregate_registry.is_aggregate_call(function)
        {
            if !self.aggregates.is_empty() {
                return ControlFlow::Break(unsupported(
                    "nested policy aggregates during terminal DISTINCT",
                ));
            }
            let mut sources = HashSet::new();
            let _: ControlFlow<()> = visit_expressions(&*expr, |node| {
                if let Some((source, _)) = source_column(node, self.sources, self.raw) {
                    sources.insert(source);
                }
                ControlFlow::Continue(())
            });
            if sources.len() > 1 {
                return ControlFlow::Break(unsupported(
                    "multi-source aggregate expression during terminal DISTINCT",
                ));
            }
            let source = if let Some(source) = sources.into_iter().next() {
                Some(source)
            } else if self.sources.len() == 1 {
                Some(TableKey::new(&self.sources[0]))
            } else if self.sources.is_empty() {
                None
            } else {
                return ControlFlow::Break(unsupported(
                    "source-unspecified aggregate over multiple policy sources during terminal DISTINCT",
                ));
            };
            self.aggregates.push((expr as *const Expr as usize, source));
            return ControlFlow::Continue(());
        }
        if let Some(key) = source_column(expr, self.sources, self.raw) {
            let value = self.raw[&key].clone();
            if self.aggregates.is_empty() {
                let present = self
                    .present
                    .get(&key.0)
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "TRUE".into());
                let masked = format!("CASE WHEN ({present}) IS TRUE THEN {value} ELSE NULL END");
                if self.guarded.insert(key) {
                    self.guards.push(format!("COUNT(DISTINCT {masked}) = 1"));
                    self.guards.push(format!("COUNT({masked}) = COUNT(CASE WHEN ({present}) IS TRUE THEN 1 ELSE NULL END)"));
                }
                match crate::sql::parse_policy_expr_duckdb(&format!("MIN({masked})")) {
                    Ok(replacement) => *expr = replacement,
                    Err(error) => return ControlFlow::Break(error),
                }
            } else {
                *expr = value;
            }
        } else if let Expr::CompoundIdentifier(parts) = expr
            && let Some(column) = QualifiedColumn::from_compound_identifier(parts)
        {
            let qualifier = TableKey::from_table(&column.table);
            if qualifier == TableKey::new("_OUTPUT_")
                || self
                    .sink
                    .is_some_and(|sink| qualifier == TableKey::new(sink))
                || self
                    .sink_alias
                    .is_some_and(|alias| qualifier == TableKey::new(alias))
            {
                let Some(value) = self.outputs.get(&column.column.key()) else {
                    return ControlFlow::Break(unsupported(
                        "unavailable output attribute during terminal DISTINCT",
                    ));
                };
                *expr = value.clone();
            }
        }
        ControlFlow::Continue(())
    }
    fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        if self
            .aggregates
            .last()
            .is_some_and(|(node, _)| *node == expr as *const Expr as usize)
        {
            let (_, source) = self.aggregates.pop().unwrap();
            if let Some(source) = source
                && let Some(present) = self.present.get(&source)
            {
                let Expr::Function(function) = expr else {
                    unreachable!()
                };
                let filter = if let Some(existing) = function.filter.take() {
                    format!("({existing}) AND (({present}) IS TRUE)")
                } else {
                    format!("({present}) IS TRUE")
                };
                match crate::sql::parse_policy_expr_duckdb(&filter) {
                    Ok(filter) => function.filter = Some(Box::new(filter)),
                    Err(error) => return ControlFlow::Break(error),
                }
            }
        }
        ControlFlow::Continue(())
    }
}
