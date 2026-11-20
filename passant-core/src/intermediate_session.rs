//! Session-owned contributor resolution and opaque persisted-state tokens.
use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Deserialize;

use crate::intermediate_state::{
    IntermediatePlan, IntermediateState, SqlExecutor, StateResult, quote,
};

const SOURCE_KEY: &str = "_dfc_intermediate_source_rowid";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntermediateDependency {
    pub t: String,
    pub r: Option<i64>,
}

pub struct IntermediateSession {
    plans: Vec<IntermediatePlan>,
    sources: HashMap<String, String>,
    states: HashMap<i64, Vec<IntermediateState>>,
    next_token: i64,
}

impl IntermediateSession {
    pub fn new(
        plans: Vec<IntermediatePlan>,
        sources: HashMap<String, String>,
    ) -> StateResult<Self> {
        for (name, canonical) in &sources {
            if name != &canonical.to_lowercase()
                || !plans.iter().any(|p| p.sources.contains(canonical))
            {
                return Err("Invalid intermediate source binding".into());
            }
        }
        if plans
            .iter()
            .flat_map(|p| &p.sources)
            .any(|s| sources.get(&s.to_lowercase()) != Some(s))
        {
            return Err("Missing or ambiguous intermediate source binding".into());
        }
        Ok(Self {
            plans,
            sources,
            states: HashMap::new(),
            next_token: 1,
        })
    }

    pub fn row_states<E: SqlExecutor>(
        &self,
        db: &mut E,
        dependencies: &[IntermediateDependency],
    ) -> StateResult<Vec<IntermediateState>> {
        let mut inherited = Vec::new();
        let mut direct: BTreeMap<&str, BTreeSet<i64>> = BTreeMap::new();
        for dependency in dependencies {
            if dependency.t == "state" {
                if let Some(token) = dependency.r {
                    inherited.push(
                        self.states
                            .get(&token)
                            .ok_or("Unknown persisted intermediate state")?,
                    );
                }
            } else if let Some(source) = dependency.t.strip_prefix("source:") {
                let canonical = self
                    .sources
                    .get(source)
                    .ok_or("Unknown intermediate source")?;
                if let Some(identity) = dependency.r {
                    direct.entry(canonical).or_default().insert(identity);
                }
            } else {
                return Err("Unknown intermediate dependency tag".into());
            }
        }
        self.plans
            .iter()
            .enumerate()
            .map(|(index, plan)| {
                let mut captured = Vec::new();
                for (source, identities) in &direct {
                    if !plan.sources.contains(*source) {
                        continue;
                    }
                    let ids = identities
                        .iter()
                        .map(i64::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    let sql = format!(
                        "SELECT *, rowid AS {} FROM {} WHERE rowid IN ({ids})",
                        quote(SOURCE_KEY),
                        quote(source)
                    );
                    captured.push(plan.capture(db, source, &sql, &[SOURCE_KEY.to_owned()])?);
                }
                let mut parts: Vec<_> = inherited.iter().map(|states| &states[index]).collect();
                parts.extend(captured.iter());
                plan.merge(db, &parts)
            })
            .collect()
    }

    fn validate_rows(&self, rows: &[Vec<IntermediateState>]) -> StateResult<()> {
        for row in rows {
            if row.len() != self.plans.len() {
                return Err("Intermediate row has a different policy set".into());
            }
            for (plan, state) in self.plans.iter().zip(row) {
                plan.validate(state)?;
            }
        }
        Ok(())
    }

    pub fn merge_rows<E: SqlExecutor>(
        &self,
        db: &mut E,
        rows: &[Vec<IntermediateState>],
    ) -> StateResult<Vec<IntermediateState>> {
        self.validate_rows(rows)?;
        self.plans
            .iter()
            .enumerate()
            .map(|(index, plan)| {
                plan.merge(db, &rows.iter().map(|row| &row[index]).collect::<Vec<_>>())
            })
            .collect()
    }

    /// Validate the entire batch before changing the registry.
    pub fn register_rows(&mut self, rows: Vec<Vec<IntermediateState>>) -> StateResult<Vec<i64>> {
        self.validate_rows(&rows)?;
        let count = i64::try_from(rows.len()).map_err(|_| "Too many intermediate rows")?;
        let next = self
            .next_token
            .checked_add(count)
            .ok_or("Intermediate state token limit reached")?;
        let tokens: Vec<_> = (self.next_token..next).collect();
        for (token, row) in tokens.iter().zip(rows) {
            self.states.insert(*token, row);
        }
        self.next_token = next;
        Ok(tokens)
    }

    pub fn checkpoint(&self) -> i64 {
        self.next_token
    }

    pub fn rollback(&mut self, checkpoint: i64) -> StateResult<()> {
        if checkpoint < 1 || checkpoint > self.next_token {
            return Err("Invalid intermediate session checkpoint".into());
        }
        self.states.retain(|token, _| *token < checkpoint);
        self.next_token = checkpoint;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoSql;
    impl SqlExecutor for NoSql {
        fn query_json(&mut self, _: &str) -> StateResult<String> {
            panic!("Unexpected SQL")
        }
        fn describe_types(&mut self, _: &str) -> StateResult<Vec<String>> {
            panic!("Unexpected SQL")
        }
    }
    fn plan() -> IntermediatePlan {
        IntermediatePlan::new(vec![], vec!["source".into()], vec![]).unwrap()
    }
    fn session(plan: IntermediatePlan) -> IntermediateSession {
        IntermediateSession::new(
            vec![plan],
            HashMap::from([("source".into(), "source".into())]),
        )
        .unwrap()
    }
    #[test]
    fn tokens_rollback_and_ownership_fail_closed() {
        let plan = plan();
        let state = plan.empty();
        let foreign = self::plan().empty();
        let mut session = session(plan);
        let checkpoint = session.checkpoint();
        assert!(
            session
                .register_rows(vec![vec![state.clone()], vec![foreign]])
                .is_err()
        );
        assert_eq!(session.checkpoint(), checkpoint);
        assert!(session.register_rows(vec![vec![]]).is_err());
        let tokens = session.register_rows(vec![vec![state]]).unwrap();
        assert_eq!(tokens, vec![1]);
        assert!(
            session
                .row_states(
                    &mut NoSql,
                    &[IntermediateDependency {
                        t: "state".into(),
                        r: Some(1)
                    }]
                )
                .is_ok()
        );
        session.rollback(checkpoint).unwrap();
        assert!(
            session
                .row_states(
                    &mut NoSql,
                    &[IntermediateDependency {
                        t: "state".into(),
                        r: Some(1)
                    }]
                )
                .is_err()
        );
        assert!(session.rollback(0).is_err());
        assert!(session.rollback(2).is_err());
    }
    #[test]
    fn unknown_tags_and_sources_are_rejected_even_for_null_dependencies() {
        let session = session(plan());
        for tag in ["unknown", "source:missing"] {
            assert!(
                session
                    .row_states(
                        &mut NoSql,
                        &[IntermediateDependency {
                            t: tag.into(),
                            r: None
                        }]
                    )
                    .is_err()
            );
        }
    }
}
