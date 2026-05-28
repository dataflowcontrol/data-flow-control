use sqlparser::ast::helpers::attached_token::AttachedToken;
use sqlparser::ast::{
    Array, BinaryOperator, Cte, DuplicateTreatment, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArgumentList, FunctionArguments, GroupByExpr, Ident, Join, JoinConstraint,
    JoinOperator, ObjectName, Query, Select, SelectItem, SetExpr, Statement, TableAlias,
    TableFactor, TableWithJoins, Value, WildcardAdditionalOptions, With,
};

/// `table.column` compound identifier.
pub fn qualified_column(table: &str, column: &str) -> Expr {
    Expr::CompoundIdentifier(vec![Ident::new(table), Ident::new(column)])
}

pub fn identifier(name: &str) -> Expr {
    Expr::Identifier(Ident::new(name))
}

pub fn comparison_op_from_str(op: &str) -> Option<BinaryOperator> {
    match op {
        ">" => Some(BinaryOperator::Gt),
        ">=" => Some(BinaryOperator::GtEq),
        "<" => Some(BinaryOperator::Lt),
        "<=" => Some(BinaryOperator::LtEq),
        "=" => Some(BinaryOperator::Eq),
        "!=" => Some(BinaryOperator::NotEq),
        _ => None,
    }
}

pub fn column_comparison(column: &str, op: &str, rhs: Expr) -> Option<Expr> {
    Some(binary_comparison(
        identifier(column),
        comparison_op_from_str(op)?,
        rhs,
    ))
}

pub fn distinct_aggregate(name: &str, arg: Expr) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![Ident::new(name)]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: Some(DuplicateTreatment::Distinct),
            args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(arg))],
            clauses: Vec::new(),
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: Vec::new(),
    })
}

pub fn count_distinct_eq_one(table: &str, column: &str) -> Expr {
    binary_comparison(
        distinct_aggregate("count", qualified_column(table, column)),
        BinaryOperator::Eq,
        Expr::Value(Value::Number("1".into(), false)),
    )
}

pub fn scalar_subquery(body: Expr, table: &str) -> Expr {
    Expr::Subquery(Box::new(query_from_select(grouped_select(
        vec![SelectItem::UnnamedExpr(body)],
        vec![TableWithJoins {
            relation: table_factor(table),
            joins: Vec::new(),
        }],
        None,
        Vec::new(),
    ))))
}

pub fn int_literal(value: i64) -> Expr {
    Expr::Value(Value::Number(value.to_string(), false))
}

pub fn is_not_null(expr: Expr) -> Expr {
    Expr::IsNotNull(Box::new(expr))
}

pub fn duckdb_array(elem: Expr) -> Expr {
    Expr::Array(Array {
        elem: vec![elem],
        named: false,
    })
}

pub fn binary_comparison(left: Expr, op: BinaryOperator, right: Expr) -> Expr {
    Expr::BinaryOp {
        left: Box::new(left),
        op,
        right: Box::new(right),
    }
}

pub fn and_exprs(mut exprs: Vec<Expr>) -> Option<Expr> {
    if exprs.is_empty() {
        return None;
    }
    let mut acc = exprs.remove(0);
    for expr in exprs {
        acc = Expr::BinaryOp {
            left: Box::new(acc),
            op: BinaryOperator::And,
            right: Box::new(expr),
        };
    }
    Some(acc)
}

pub fn ident(name: &str) -> Ident {
    Ident::new(name)
}

pub fn object_name(name: &str) -> ObjectName {
    ObjectName(vec![Ident::new(name)])
}

pub fn table_alias(name: &str) -> TableAlias {
    TableAlias {
        name: Ident::new(name),
        columns: Vec::new(),
    }
}

pub fn table_factor(name: &str) -> TableFactor {
    TableFactor::Table {
        name: object_name(name),
        alias: None,
        args: None,
        with_hints: Vec::new(),
        version: None,
        with_ordinality: false,
        partitions: Vec::new(),
        json_path: None,
    }
}

pub fn qualified_wildcard(table: &str) -> SelectItem {
    SelectItem::QualifiedWildcard(object_name(table), WildcardAdditionalOptions::default())
}

pub fn alias_expr(expr: Expr, alias: &str) -> SelectItem {
    SelectItem::ExprWithAlias {
        expr,
        alias: Ident::new(alias),
    }
}

pub fn alias_column(table: &str, column: &str, alias: &str) -> SelectItem {
    alias_expr(qualified_column(table, column), alias)
}

pub fn empty_select() -> Select {
    Select {
        select_token: AttachedToken::empty(),
        distinct: None,
        top: None,
        top_before_distinct: false,
        projection: Vec::new(),
        into: None,
        from: Vec::new(),
        lateral_views: Vec::new(),
        prewhere: None,
        selection: None,
        group_by: GroupByExpr::Expressions(Vec::new(), Vec::new()),
        cluster_by: Vec::new(),
        distribute_by: Vec::new(),
        sort_by: Vec::new(),
        having: None,
        named_window: Vec::new(),
        qualify: None,
        window_before_qualify: false,
        value_table_mode: None,
        connect_by: None,
    }
}

pub fn query_from_select(select: Select) -> Query {
    Query {
        with: None,
        body: Box::new(SetExpr::Select(Box::new(select))),
        order_by: None,
        limit: None,
        limit_by: Vec::new(),
        offset: None,
        fetch: None,
        locks: Vec::new(),
        for_clause: None,
        settings: None,
        format_clause: None,
    }
}

pub fn cte(name: &str, query: Query) -> Cte {
    Cte {
        alias: table_alias(name),
        query: Box::new(query),
        from: None,
        materialized: None,
        closing_paren_token: AttachedToken::empty(),
    }
}

pub fn with_ctes(ctes: Vec<Cte>, body: SetExpr) -> Query {
    Query {
        with: Some(With {
            with_token: AttachedToken::empty(),
            recursive: false,
            cte_tables: ctes,
        }),
        body: Box::new(body),
        order_by: None,
        limit: None,
        limit_by: Vec::new(),
        offset: None,
        fetch: None,
        locks: Vec::new(),
        for_clause: None,
        settings: None,
        format_clause: None,
    }
}

pub fn partial_push_join_from(base: &str, policy: &str, key_names: &[String]) -> TableWithJoins {
    let base_factor = table_factor(base);
    if key_names.is_empty() {
        return TableWithJoins {
            relation: base_factor,
            joins: vec![Join {
                relation: table_factor(policy),
                global: false,
                join_operator: JoinOperator::CrossJoin,
            }],
        };
    }
    let conditions = key_names
        .iter()
        .map(|key| {
            binary_comparison(
                qualified_column(base, key),
                BinaryOperator::Eq,
                qualified_column(policy, key),
            )
        })
        .collect::<Vec<_>>();
    let on = and_exprs(conditions).expect("join keys should not be empty");
    TableWithJoins {
        relation: base_factor,
        joins: vec![Join {
            relation: table_factor(policy),
            global: false,
            join_operator: JoinOperator::Inner(JoinConstraint::On(on)),
        }],
    }
}

pub fn partial_push_split_query(
    base_query: Query,
    policy_eval: Query,
    join_keys: &[String],
) -> Query {
    let mut select = empty_select();
    select.projection = vec![qualified_wildcard(BASE_QUERY_CTE)];
    select.from = vec![partial_push_join_from(
        BASE_QUERY_CTE,
        POLICY_EVAL_CTE,
        join_keys,
    )];
    with_ctes(
        vec![
            cte(BASE_QUERY_CTE, base_query),
            cte(POLICY_EVAL_CTE, policy_eval),
        ],
        SetExpr::Select(Box::new(select)),
    )
}

pub fn statement_from_query(query: Query) -> Statement {
    Statement::Query(Box::new(query))
}

pub fn function_call(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![Ident::new(name)]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: args
                .into_iter()
                .map(|expr| FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)))
                .collect(),
            clauses: Vec::new(),
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: Vec::new(),
    })
}

pub fn grouped_select(
    projection: Vec<SelectItem>,
    from: Vec<TableWithJoins>,
    selection: Option<Expr>,
    group_by: Vec<Expr>,
) -> Select {
    let mut select = empty_select();
    select.projection = projection;
    select.from = from;
    select.selection = selection;
    select.group_by = GroupByExpr::Expressions(group_by, Vec::new());
    select
}

pub fn string_literal(value: &str) -> Expr {
    Expr::Value(Value::SingleQuotedString(value.to_string()))
}

pub fn null_literal() -> Expr {
    Expr::Value(Value::Null)
}

pub fn bool_literal(value: bool) -> Expr {
    Expr::Value(Value::Boolean(value))
}

pub fn case_when(condition: Expr, then_expr: Expr, else_expr: Expr) -> Expr {
    Expr::Case {
        operand: None,
        conditions: vec![condition],
        results: vec![then_expr],
        else_result: Some(Box::new(else_expr)),
    }
}

pub fn or_kill(predicate: Expr) -> Expr {
    Expr::BinaryOp {
        left: Box::new(Expr::Nested(Box::new(predicate))),
        op: BinaryOperator::Or,
        right: Box::new(function_call("kill", Vec::new())),
    }
}

pub fn string_concat(left: Expr, right: Expr) -> Expr {
    Expr::BinaryOp {
        left: Box::new(left),
        op: BinaryOperator::StringConcat,
        right: Box::new(right),
    }
}

pub fn wrap_table_with_filter(
    relation: TableFactor,
    predicate: Expr,
    alias_name: &str,
) -> TableFactor {
    let select = grouped_select(
        vec![SelectItem::Wildcard(WildcardAdditionalOptions::default())],
        vec![TableWithJoins {
            relation,
            joins: Vec::new(),
        }],
        Some(predicate),
        Vec::new(),
    );
    TableFactor::Derived {
        lateral: false,
        subquery: Box::new(query_from_select(select)),
        alias: Some(table_alias(alias_name)),
    }
}

const BASE_QUERY_CTE: &str = "base_query";
const POLICY_EVAL_CTE: &str = "policy_eval";

/// Stable internal name for Passant-generated SQL artifacts (temp columns, CTE aliases).
pub fn passant_internal_name(prefix: &str, suffix: &str) -> String {
    format!("{prefix}_{suffix}")
}

pub fn passant_filter_temp_column(column: &str) -> String {
    passant_internal_name("__passant_filter", column)
}

/// Sanitize a derived SELECT item alias (SQL identifier rules, max 50 chars).
pub fn sanitize_projection_alias(raw: &str) -> String {
    let mut alias = raw
        .to_ascii_lowercase()
        .replace(['(', ')'], "_")
        .replace([',', ' '], "_");
    alias = alias
        .replace('.', "_")
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    if alias.is_empty() {
        return "expr".to_string();
    }
    if alias
        .chars()
        .next()
        .is_some_and(|ch| !ch.is_ascii_alphabetic())
    {
        alias = passant_internal_name("expr", &alias);
    }
    alias.chars().take(50).collect()
}
