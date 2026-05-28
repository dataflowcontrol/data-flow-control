//! Annotation column construction for per-tuple source-set tracking.
//!
//! Outer joins and set operations that mix multi-source policies may require
//! synthetic annotation columns so enforcement predicates remain row-local.

use sqlparser::ast::Select;

use super::analysis::{select_has_anti_join, select_has_full_join, select_nullable_source_tables};

/// Returns whether a SELECT scope may need source-set annotation columns.
#[allow(dead_code)]
pub fn select_requires_source_set_annotations(select: &Select) -> bool {
    select_has_full_join(select)
        || select_has_anti_join(select)
        || !select_nullable_source_tables(select).is_empty()
}
