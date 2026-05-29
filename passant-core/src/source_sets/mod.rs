//! Source-set analysis and policy splitting for outer joins, set operations, and
//! other queries where cross-source policies need branch-local or row-level handling.

mod analysis;
mod annotations;
mod rewrites;
mod split;

pub use analysis::{
    policy_requires_set_split, select_has_anti_join, select_has_full_join,
    select_nullable_source_tables, select_source_tables, set_expr_source_tables,
    set_operation_requires_cross_source_policies, table_factor_source_tables,
    table_with_joins_source_tables,
};
pub use rewrites::{
    cross_source_policies_for_branch, split_policy_for_set_branches, split_set_operation_policies,
    split_set_operation_policies_for_store,
};
pub use split::{
    split_policy_by_source_local_conjuncts, split_select_policies_for_nullable_joins,
    split_select_policies_for_nullable_joins_for_store,
};

pub(crate) use analysis::set_operation_requires_cross_source_policies_for_store;
pub(crate) use rewrites::cross_source_policies_for_branch_indexed;
pub(crate) use split::{compile_constraint_referenced_source_keys, compile_source_local_conjuncts};

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use sqlparser::ast::{SetExpr, Statement};

    use crate::parser::parse_query;
    use crate::policy::{PolicyIr, Resolution};

    use super::*;

    fn select_from_sql(sql: &str) -> sqlparser::ast::Select {
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
        assert!(nullable.contains(&crate::identifiers::TableKey::new("foo")));
        assert!(!nullable.contains(&crate::identifiers::TableKey::new("bar")));
    }

    #[test]
    fn set_operation_detects_cross_source_policy() {
        let left = set_expr_from_sql("SELECT id FROM foo");
        let right = set_expr_from_sql("SELECT id FROM bar");
        let policy = PolicyIr::Pgn {
            sources: vec!["foo".to_string(), "bar".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
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
        let policy = PolicyIr::Pgn {
            sources: vec!["bar".to_string(), "foo".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "max(bar.id) > 1 AND max(foo.id) > 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        };
        let available = HashSet::from([
            crate::identifiers::TableKey::new("bar"),
            crate::identifiers::TableKey::new("foo"),
        ]);
        let split = split_policy_by_source_local_conjuncts(&policy, None, None, &available)
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
        let policy = PolicyIr::Pgn {
            sources: vec!["foo".to_string(), "bar".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
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
        assert!(tables.contains(&crate::identifiers::TableKey::new("foo")));
        assert!(tables.contains(&crate::identifiers::TableKey::new("bar")));
    }
}
