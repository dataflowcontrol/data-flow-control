//! Native statement and effect planning for opaque intermediate outputs.
//!
//! Database execution stays in the adapter. Policy identity, sink applicability,
//! resolution precedence and permitted SQL effects are core decisions.
use std::collections::{BTreeMap, HashSet};

use serde::Serialize;
use sqlparser::ast::{HiveDistributionStyle, ObjectName, Statement};
use sqlparser::dialect::DuckDbDialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer};

use crate::diagnostics::RewriteError;
use crate::policy::Resolution;
use crate::rewriter::PassantRewriter;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IntermediateStatementPlan {
    Select {
        query_sql: String,
    },
    Insert {
        target: String,
        columns: Vec<String>,
        query_sql: String,
    },
    Create {
        target: String,
        temporary: bool,
        query_sql: Option<String>,
        ddl_sql: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IntermediateRetryFailure {
    pub policy_index: usize,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IntermediateResolution {
    Keep {
        indices: Vec<usize>,
    },
    Kill,
    Retry {
        failures: Vec<IntermediateRetryFailure>,
    },
}

fn error(message: &str) -> RewriteError {
    RewriteError::unsupported_statement(format!("intermediate execution: {message}"))
}

impl PassantRewriter {
    fn intermediate_target(&self, name: &ObjectName) -> Result<String, RewriteError> {
        if name.0.len() != 1 {
            return Err(error(
                "Automatic propagation requires unqualified physical table names",
            ));
        }
        let target = name.0[0].value.clone();
        if self
            .catalog
            .intermediate_binding(&target)
            .is_some_and(|binding| binding.source.is_some())
        {
            return Err(error(
                "Ordinary SQL cannot write or shadow a declared SOURCE relation; authoritative observations must come from the trusted harness",
            ));
        }
        Ok(target)
    }

    /// Compile one database effect. Generated hidden columns are introduced only
    /// after all original identifiers and the target authority have been checked.
    pub fn plan_intermediate_statement(
        &self,
        sql: &str,
    ) -> Result<IntermediateStatementPlan, RewriteError> {
        let dialect = DuckDbDialect {};
        let tokens = Tokenizer::new(&dialect, sql)
            .tokenize()
            .map_err(|e| error(&e.to_string()))?;
        if tokens.iter().any(|token| matches!(token, Token::Word(word) if matches!(word.value.to_ascii_lowercase().as_str(), "_dfc_intermediate_state" | "_dfc_intermediate_dependencies" | "_dfc_intermediate_source_rowid"))) {
            return Err(error("Intermediate metadata columns are reserved"));
        }
        let mut statements = Parser::parse_sql(&dialect, sql).map_err(|e| error(&e.to_string()))?;
        if statements.len() != 1 {
            return Err(error(
                "Automatic propagation requires one statement per execute",
            ));
        }
        let statement = statements.remove(0);
        match &statement {
            Statement::Query(query) => Ok(IntermediateStatementPlan::Select {
                query_sql: self.rewrite_intermediate_query(&query.to_string())?,
            }),
            Statement::Insert(insert) => {
                let target = self.intermediate_target(&insert.table_name)?;
                if insert.or.is_some()
                    || insert.ignore
                    || insert.overwrite
                    || insert.partitioned.is_some()
                    || !insert.after_columns.is_empty()
                    || insert.on.is_some()
                    || insert.returning.is_some()
                    || insert.replace_into
                    || insert.priority.is_some()
                    || insert.insert_alias.is_some()
                    || insert.table_alias.is_some()
                {
                    return Err(error(
                        "INSERT RETURNING/conflict/overwrite/aliases is unsupported",
                    ));
                }
                let logical = self
                    .catalog
                    .columns(&target)
                    .ok_or_else(|| error("missing catalog relation"))?;
                let columns = if insert.columns.is_empty() {
                    logical.to_vec()
                } else {
                    insert
                        .columns
                        .iter()
                        .map(|c| c.value.clone())
                        .collect::<Vec<_>>()
                };
                let names = columns
                    .iter()
                    .map(|c| c.to_ascii_lowercase())
                    .collect::<HashSet<_>>();
                let expected = logical
                    .iter()
                    .map(|c| c.to_ascii_lowercase())
                    .collect::<HashSet<_>>();
                if columns.len() != logical.len() || names != expected {
                    return Err(error(
                        "Intermediate INSERT must supply every logical target column exactly once",
                    ));
                }
                // Resolve actual catalog spelling for the adapter's target casts.
                let columns = columns
                    .into_iter()
                    .map(|name| {
                        logical
                            .iter()
                            .find(|c| c.eq_ignore_ascii_case(&name))
                            .unwrap()
                            .clone()
                    })
                    .collect();
                let query = insert
                    .source
                    .as_ref()
                    .ok_or_else(|| error("INSERT SELECT is required"))?;
                Ok(IntermediateStatementPlan::Insert {
                    target,
                    columns,
                    query_sql: self.rewrite_intermediate_query(&query.to_string())?,
                })
            }
            Statement::CreateTable(create) => {
                let target = self.intermediate_target(&create.name)?;
                if create.or_replace || create.if_not_exists {
                    return Err(error("CREATE OR REPLACE / IF NOT EXISTS is unsupported"));
                }
                if let Some(query) = &create.query {
                    if !create.columns.is_empty() || !create.constraints.is_empty() {
                        return Err(error("CTAS column lists and constraints are unsupported"));
                    }
                    if create.external
                        || create.global.is_some()
                        || create.transient
                        || create.volatile
                        || !matches!(create.hive_distribution, HiveDistributionStyle::NONE)
                        || create.hive_formats.as_ref().is_some_and(|format| {
                            format.row_format.is_some()
                                || format.serde_properties.is_some()
                                || format.storage.is_some()
                                || format.location.is_some()
                        })
                        || !create.table_properties.is_empty()
                        || !create.with_options.is_empty()
                        || create.file_format.is_some()
                        || create.location.is_some()
                        || create.without_rowid
                        || create.like.is_some()
                        || create.clone.is_some()
                        || create.engine.is_some()
                        || create.comment.is_some()
                        || create.auto_increment_offset.is_some()
                        || create.default_charset.is_some()
                        || create.collation.is_some()
                        || create.on_commit.is_some()
                        || create.on_cluster.is_some()
                        || create.primary_key.is_some()
                        || create.order_by.is_some()
                        || create.partition_by.is_some()
                        || create.cluster_by.is_some()
                        || create.clustered_by.is_some()
                        || create.options.is_some()
                        || create.strict
                        || create.copy_grants
                        || create.enable_schema_evolution.is_some()
                        || create.change_tracking.is_some()
                        || create.data_retention_time_in_days.is_some()
                        || create.max_data_extension_time_in_days.is_some()
                        || create.default_ddl_collation.is_some()
                        || create.with_aggregation_policy.is_some()
                        || create.with_row_access_policy.is_some()
                        || create.with_tags.is_some()
                    {
                        return Err(error("CTAS supports only the TEMPORARY table property"));
                    }
                    Ok(IntermediateStatementPlan::Create {
                        target,
                        temporary: create.temporary,
                        query_sql: Some(self.rewrite_intermediate_query(&query.to_string())?),
                        ddl_sql: None,
                    })
                } else {
                    // LIKE/CLONE could copy hidden columns from a tracked relation.
                    if create.like.is_some()
                        || create.clone.is_some()
                        || create.external
                        || create.location.is_some()
                    {
                        return Err(error("CREATE TABLE must declare its own logical schema"));
                    }
                    Ok(IntermediateStatementPlan::Create {
                        target,
                        temporary: create.temporary,
                        query_sql: None,
                        ddl_sql: Some(statement.to_string()),
                    })
                }
            }
            _ => Err(error(
                "Automatic propagation supports SELECT, CREATE TABLE, CTAS and INSERT SELECT only; other statements cannot silently discard or mutate provenance",
            )),
        }
    }

    fn validate_intermediate_resolutions(&self) -> Result<(), RewriteError> {
        for index in self.store.policy_indices() {
            if let Some(policy) = self.store.policy(index)
                && !matches!(
                    policy.resolution(),
                    Resolution::Kill | Resolution::Remove | Resolution::Retry(_)
                )
            {
                return Err(error(
                    "Automatic intermediate execution supports REMOVE, KILL and RETRY only",
                ));
            }
        }
        Ok(())
    }

    /// Stable policy identities applicable to this endpoint; contributor/source
    /// applicability is evaluated by the native intermediate state plan.
    pub fn intermediate_policy_indices(
        &self,
        sink: Option<&str>,
    ) -> Result<Vec<usize>, RewriteError> {
        self.validate_intermediate_resolutions()?;
        Ok(self
            .store
            .policy_indices()
            .into_iter()
            .filter_map(|index| {
                let policy = self.store.policy(index)?;
                if policy.sink().is_some_and(|required| {
                    !sink.is_some_and(|actual| actual.eq_ignore_ascii_case(required))
                }) {
                    None
                } else {
                    self.store.original_policy_index(index)
                }
            })
            .collect())
    }

    /// Use the ordinary output-schema applicability rule before binding state
    /// predicates. INSERT callers supply target names rather than SELECT labels.
    pub fn intermediate_policy_indices_for_output(
        &self,
        sink: Option<&str>,
        outputs: &[String],
    ) -> Result<Vec<usize>, RewriteError> {
        let mut applicable = Vec::new();
        let eligible: HashSet<_> = self
            .intermediate_policy_indices(sink)?
            .into_iter()
            .collect();
        for local_index in self.store.policy_indices() {
            let compiled = self
                .store
                .compiled(local_index)
                .expect("registered intermediate policy");
            let index = compiled.original_policy_index;
            if !eligible.contains(&index) {
                continue;
            }
            let required = crate::rewriter::output_scope::required_columns(
                &compiled.policy,
                &compiled
                    .constraint
                    .as_ref()
                    .expect("compiled constraint")
                    .ast,
            );
            if crate::rewriter::output_scope::output_columns_available(&required, outputs)? {
                applicable.push(index);
            }
        }
        Ok(applicable)
    }

    /// Apply existing registry precedence to already-evaluated native predicates.
    /// All rows are inspected for KILL before a RETRY decision can escape.
    pub fn resolve_intermediate_rows(
        &self,
        sink: Option<&str>,
        policy_indices: &[usize],
        verdicts: &[Vec<Option<bool>>],
    ) -> Result<IntermediateResolution, RewriteError> {
        self.validate_intermediate_resolutions()?;
        if policy_indices != self.registered_policy_indices() {
            return Err(error(
                "Policy changes require a new intermediate execution session",
            ));
        }
        if verdicts.iter().any(|row| row.len() != policy_indices.len()) {
            return Err(error(
                "Intermediate verdicts must match the registered policy set",
            ));
        }
        let applicable = self
            .intermediate_policy_indices(sink)?
            .into_iter()
            .collect::<HashSet<_>>();
        let mut order = policy_indices
            .iter()
            .enumerate()
            .filter(|(_, index)| applicable.contains(index))
            .map(|(column, index)| {
                let policy = self
                    .store
                    .policy_by_original_index(*index)
                    .expect("validated stable policy identity");
                (column, *index, policy.resolution())
            })
            .collect::<Vec<_>>();
        order.sort_by_key(|(_, index, resolution)| {
            (
                self.resolution_registry.properties(resolution).precedence,
                *index,
            )
        });
        let mut indices = Vec::new();
        let mut failures = BTreeMap::new();
        for (row_index, row) in verdicts.iter().enumerate() {
            let mut removed = false;
            for (column, policy_index, resolution) in &order {
                if row[*column] != Some(false) {
                    continue;
                }
                match resolution {
                    Resolution::Kill => return Ok(IntermediateResolution::Kill),
                    Resolution::Remove => {
                        removed = true;
                        break;
                    }
                    Resolution::Retry(message) => {
                        failures.insert(*policy_index, message.clone());
                    }
                    _ => unreachable!("validated intermediate resolution"),
                }
            }
            if !removed {
                indices.push(row_index);
            }
        }
        if !failures.is_empty() {
            return Ok(IntermediateResolution::Retry {
                failures: failures
                    .into_iter()
                    .map(|(policy_index, message)| IntermediateRetryFailure {
                        policy_index,
                        message,
                    })
                    .collect(),
            });
        }
        Ok(IntermediateResolution::Keep { indices })
    }
}
