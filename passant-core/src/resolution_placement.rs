//! Pure effect-placement metadata. Predicate/evidence propagation is distinct
//! from invoking effects: every non-splittable effect belongs to a joint stage.
use crate::{
    Resolution,
    resolution_registry::{ResolutionPrecedence, ResolutionRegistry},
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolutionPlacement {
    pub joint_policy_indices: Vec<usize>,
    pub independently_splittable_policy_indices: Vec<usize>,
}

impl ResolutionPlacement {
    pub fn for_policies<'a>(
        policies: impl IntoIterator<Item = (usize, &'a Resolution)>,
        registry: &ResolutionRegistry,
    ) -> Self {
        let policies: Vec<_> = policies.into_iter().collect();
        let unknown_joint_precedence = policies.iter().any(|(_, r)| {
            let p = registry.properties(r);
            !p.splittable && p.precedence.is_none()
        });
        let highest_joint: Option<ResolutionPrecedence> = policies
            .iter()
            .filter_map(|(_, r)| {
                let p = registry.properties(r);
                (!p.splittable).then_some(p.precedence).flatten()
            })
            .min();
        let mut result = Self::default();
        for (id, resolution) in policies {
            let p = registry.properties(resolution);
            // A lower-precedence effect cannot run before an unresolved higher stage.
            if !unknown_joint_precedence
                && p.splittable
                && p.precedence.is_some()
                && highest_joint.is_none_or(|joint| p.precedence.unwrap() < joint)
            {
                result.independently_splittable_policy_indices.push(id);
            } else {
                result.joint_policy_indices.push(id);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_non_splittable_kinds_stay_joint_and_lower_splittable_waits() {
        let mut registry = ResolutionRegistry::default();
        registry.register("split_repair", true);
        let resolutions = [
            Resolution::Kill,
            Resolution::Remove,
            Resolution::Retry("x".into()),
            Resolution::Udf("legacy".into()),
            Resolution::Udf("split_repair".into()),
        ];
        let plan = ResolutionPlacement::for_policies(resolutions.iter().enumerate(), &registry);
        assert_eq!(plan.independently_splittable_policy_indices, vec![0, 1]);
        assert_eq!(plan.joint_policy_indices, vec![2, 3, 4]);
    }
}
