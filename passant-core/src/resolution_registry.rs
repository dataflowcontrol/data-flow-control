//! Resolution effects have placement properties separate from policy predicates.
use crate::policy::Resolution;
use std::collections::HashMap;

/// Ordered from highest to lowest precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResolutionPrecedence {
    Kill,
    Remove,
    Retry,
    Udf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionProperties {
    pub splittable: bool,
    pub precedence: Option<ResolutionPrecedence>,
}

#[derive(Debug, Clone)]
struct UdfDefinition {
    splittable: bool,
    ordinal: usize,
}

/// Definition ordering is independent of policy registration ordering. Clones and
/// branch views preserve ordinals; updating metadata does not redefine a function.
#[derive(Debug, Clone, Default)]
pub struct ResolutionRegistry {
    definitions: HashMap<String, UdfDefinition>,
    next_ordinal: usize,
}

impl ResolutionRegistry {
    pub fn register(&mut self, name: impl AsRef<str>, splittable: bool) -> usize {
        let key = name.as_ref().to_ascii_lowercase();
        if let Some(definition) = self.definitions.get_mut(&key) {
            definition.splittable = splittable;
            return definition.ordinal;
        }
        let ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        self.definitions.insert(
            key,
            UdfDefinition {
                splittable,
                ordinal,
            },
        );
        ordinal
    }

    pub fn registration_order(&self, name: &str) -> Option<usize> {
        self.definitions
            .get(&name.to_ascii_lowercase())
            .map(|d| d.ordinal)
    }

    pub fn properties(&self, resolution: &Resolution) -> ResolutionProperties {
        use ResolutionPrecedence as P;
        match resolution {
            Resolution::Kill => ResolutionProperties {
                splittable: true,
                precedence: Some(P::Kill),
            },
            Resolution::Remove => ResolutionProperties {
                splittable: true,
                precedence: Some(P::Remove),
            },
            Resolution::Retry(_) => ResolutionProperties {
                splittable: false,
                precedence: Some(P::Retry),
            },
            Resolution::Udf(name) | Resolution::RelationUdf(name) => ResolutionProperties {
                splittable: self
                    .definitions
                    .get(&name.to_ascii_lowercase())
                    .is_some_and(|d| d.splittable),
                precedence: Some(P::Udf),
            },
            Resolution::Ui => ResolutionProperties {
                splittable: false,
                precedence: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn definition_order_and_metadata_survive_clone() {
        let mut registry = ResolutionRegistry::default();
        assert_eq!(registry.register("double", false), 0);
        assert_eq!(registry.register("add", true), 1);
        assert_eq!(registry.register("DOUBLE", true), 0);
        let clone = registry.clone();
        assert_eq!(clone.registration_order("ADD"), Some(1));
        assert!(
            clone
                .properties(&Resolution::Udf("double".into()))
                .splittable
        );
        assert!(
            !clone
                .properties(&Resolution::Udf("legacy".into()))
                .splittable
        );
        assert!(clone.properties(&Resolution::Kill).splittable);
        assert!(
            !clone
                .properties(&Resolution::Retry("message".into()))
                .splittable
        );
    }
}
