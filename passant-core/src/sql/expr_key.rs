//! Normalized expression keys for brittle string comparisons.
//!
//! Passant compares rendered SQL fragments during rewrite planning (GROUP BY matching,
//! join-key stripping, literal checks). Route those comparisons through this module
//! instead of calling `Expr::to_string()` directly in rewrite helpers.
//!
//! Output-boundary rendering for rewritten queries still goes through `render.rs`.

use sqlparser::ast::Expr;

use super::render_expr;

/// Stable string key for expression equality checks during rewrite planning.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExprKey(String);

impl ExprKey {
    pub fn from_expr(expr: &Expr) -> Self {
        Self(render_expr(expr, None))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&Expr> for ExprKey {
    fn from(expr: &Expr) -> Self {
        Self::from_expr(expr)
    }
}

/// Compare two expressions using the same rendered key.
pub fn expr_keys_equal(left: &Expr, right: &Expr) -> bool {
    ExprKey::from_expr(left) == ExprKey::from_expr(right)
}

/// Compare a rendered expression key to an already-rendered SQL fragment.
pub fn expr_key_matches_str(expr: &Expr, rendered: &str) -> bool {
    ExprKey::from_expr(expr).as_str() == rendered
}
