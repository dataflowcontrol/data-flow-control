//! Source-set analysis and policy splitting for outer joins, set operations, and
//! other queries where cross-source policies need branch-local or row-level handling.

mod analysis;
mod annotations;
mod branch;
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
    fn outer_join_does_not_narrow_multi_source_policy_scope() {
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
        assert!(split_policy_by_source_local_conjuncts(&policy, None, None, &available).is_none());
    }

    #[test]
    fn disjoint_set_branches_do_not_apply_partial_source_scope() {
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
        assert!(left_policies.is_empty());
        assert!(right_policies.is_empty());
    }

    #[test]
    fn complete_branch_keeps_whole_policy_and_required_contract_is_unchanged() {
        let policy = crate::policy::parse_policy_text(
            "SOURCE foo, bar CONSTRAINT max(foo.id) > 1 AND max(bar.id) > 1 ON FAIL REMOVE",
        )
        .unwrap();
        let complete = set_expr_from_sql("SELECT foo.id FROM foo CROSS JOIN bar");
        let partial = set_expr_from_sql("SELECT id FROM foo");
        let (left, right) =
            split_set_operation_policies(std::slice::from_ref(&policy), &complete, &partial)
                .unwrap();
        assert_eq!(left, vec![policy]);
        assert!(right.is_empty());
        let required = crate::policy::parse_policy_text(
            "SOURCE REQUIRED foo, bar SINK target CONSTRAINT max(foo.id) > 1 ON FAIL REMOVE",
        )
        .unwrap();
        assert!(
            split_policy_for_set_branches(
                &required,
                None,
                &set_expr_source_tables(&complete),
                &set_expr_source_tables(&partial)
            )
            .is_none()
        );
    }

    #[test]
    fn union_source_tables_include_both_branches() {
        let set_expr = set_expr_from_sql("SELECT id FROM foo UNION ALL SELECT id FROM bar");
        let tables = set_expr_source_tables(&set_expr);
        assert!(tables.contains(&crate::identifiers::TableKey::new("foo")));
        assert!(tables.contains(&crate::identifiers::TableKey::new("bar")));
    }
}
