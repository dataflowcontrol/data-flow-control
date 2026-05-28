#![allow(dead_code)]

use passant_core::{
    PassantPlanner, PassantRewriter, PlanQueryResult, PolicyIr, Resolution, RewriteError,
    RewriteStrategy, TableCatalog, parse_query_to_ir,
};

pub fn dfc_policy(sources: &[&str], constraint: &str) -> PolicyIr {
    dfc_policy_with(sources, constraint, Resolution::Remove)
}

pub fn dfc_policy_with(sources: &[&str], constraint: &str, on_fail: Resolution) -> PolicyIr {
    PolicyIr::Dfc {
        sources: sources.iter().map(|s| (*s).to_string()).collect(),
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: constraint.to_string(),
        on_fail,
        description: None,
    }
}

pub fn dfc_policy_sink(sources: &[&str], sink: &str, constraint: &str) -> PolicyIr {
    PolicyIr::Dfc {
        sources: sources.iter().map(|s| (*s).to_string()).collect(),
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some(sink.to_string()),
        sink_alias: None,
        constraint: constraint.to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }
}

pub fn dfc_policy_required(sources: &[&str], required: &[&str], constraint: &str) -> PolicyIr {
    PolicyIr::Dfc {
        sources: sources.iter().map(|s| (*s).to_string()).collect(),
        required_sources: required.iter().map(|s| (*s).to_string()).collect(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: constraint.to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }
}

pub fn rewriter_with_policies(policies: &[PolicyIr]) -> PassantRewriter {
    let mut rewriter = PassantRewriter::new();
    for policy in policies {
        rewriter.register_policy(policy.clone());
    }
    rewriter
}

pub fn rewrite(sql: &str, policies: &[PolicyIr]) -> String {
    rewrite_result(sql, policies).unwrap_or_else(|err| panic!("rewrite failed for {sql:?}: {err}"))
}

pub fn rewrite_result(sql: &str, policies: &[PolicyIr]) -> Result<String, RewriteError> {
    rewriter_with_policies(policies).rewrite(sql)
}

pub fn rewrite_with_catalog(sql: &str, policies: &[PolicyIr], catalog: TableCatalog) -> String {
    let mut rewriter = PassantRewriter::with_catalog(catalog);
    for policy in policies {
        rewriter.register_policy(policy.clone());
    }
    rewriter
        .rewrite(sql)
        .unwrap_or_else(|err| panic!("rewrite failed for {sql:?}: {err}"))
}

pub fn assert_rewrite(sql: &str, policies: &[PolicyIr], expected: &str) {
    let actual = rewrite(sql, policies);
    pretty_assertions::assert_eq!(actual, expected);
}

pub fn assert_rewrite_fails_with(sql: &str, policies: &[PolicyIr], expected_substring: &str) {
    let err = rewrite_result(sql, policies).expect_err("rewrite should fail");
    let message = err.to_string();
    assert!(
        message.contains(expected_substring),
        "expected error containing {expected_substring:?}, got {message:?}"
    );
}

pub fn plan_query(sql: &str, policies: &[PolicyIr]) -> PlanQueryResult {
    let ir = parse_query_to_ir(sql).expect("query should parse");
    PassantPlanner::new().plan_query(&ir, policies)
}

pub fn assert_explain_strategy(sql: &str, policies: &[PolicyIr], strategy: RewriteStrategy) {
    assert_eq!(plan_query(sql, policies).chosen.strategy, strategy);
}

pub fn dfc_policy_kill(sources: &[&str], constraint: &str) -> PolicyIr {
    dfc_policy_with(sources, constraint, Resolution::Kill)
}
