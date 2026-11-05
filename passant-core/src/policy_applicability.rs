//! Shared source-presence semantics for query planning and persisted states.
//!
//! Sink matching belongs to the caller's binding scope. Once that matches,
//! ordinary sources decide whether a policy applies before REQUIRED sources
//! decide whether an applicable endpoint must reject a missing contributor.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyApplicability {
    Normal,
    RequiredSourceMissing,
}

pub fn source_applicability<T: PartialEq>(
    sources: &[T],
    required_sources: &[T],
    present: impl Fn(&T) -> bool,
    sink_endpoint: bool,
    allow_partial_source_visibility: bool,
) -> Option<PolicyApplicability> {
    if sink_endpoint {
        if !sources
            .iter()
            .all(|source| required_sources.contains(source) || present(source))
        {
            return None;
        }
        return Some(if required_sources.iter().any(|source| !present(source)) {
            PolicyApplicability::RequiredSourceMissing
        } else {
            PolicyApplicability::Normal
        });
    }
    if sources.iter().all(&present)
        || (allow_partial_source_visibility
            && required_sources.is_empty()
            && sources.len() > 1
            && sources.iter().any(present))
    {
        Some(PolicyApplicability::Normal)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_sources_gate_required_source_failure() {
        for visible in [vec![], vec!["a"]] {
            assert_eq!(
                source_applicability(&["a", "b"], &["a"], |s| visible.contains(s), true, false),
                None
            );
        }
        assert_eq!(
            source_applicability(&["a", "b"], &["a"], |s| *s == "b", true, false),
            Some(PolicyApplicability::RequiredSourceMissing)
        );
        assert_eq!(
            source_applicability(&["a", "b"], &["a"], |_| true, true, false),
            Some(PolicyApplicability::Normal)
        );
    }

    #[test]
    fn source_only_discovery_preserves_partial_visibility_rules() {
        assert_eq!(
            source_applicability(&["a", "b"], &[], |s| *s == "a", false, true),
            Some(PolicyApplicability::Normal)
        );
        assert_eq!(
            source_applicability(&["a", "b"], &[], |s| *s == "a", false, false),
            None
        );
        assert_eq!(
            source_applicability(&["a", "b"], &["b"], |s| *s == "a", false, true),
            None
        );
        assert_eq!(
            source_applicability(&["a"], &["a"], |_| false, false, false),
            None
        );
        assert_eq!(
            source_applicability(&["a"], &["a"], |_| false, true, false),
            Some(PolicyApplicability::RequiredSourceMissing)
        );
    }
}
