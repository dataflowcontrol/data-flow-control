use std::collections::HashMap;
use std::sync::Arc;

/// Deduplicates frequently repeated SQL fragments at registration time.
#[derive(Debug, Default, Clone)]
pub struct StringInterner {
    strings: HashMap<String, Arc<str>>,
}

impl StringInterner {
    pub fn intern(&mut self, value: &str) -> Arc<str> {
        if let Some(existing) = self.strings.get(value) {
            return existing.clone();
        }
        let shared: Arc<str> = Arc::from(value);
        self.strings.insert(value.to_string(), shared.clone());
        shared
    }

    pub fn unique_count(&self) -> usize {
        self.strings.len()
    }

    pub fn retained_key_bytes(&self) -> usize {
        self.strings.keys().map(|key| key.len()).sum()
    }

    pub fn shared_value_bytes(&self) -> usize {
        self.strings.values().map(|value| value.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interner_reuses_arc_allocation() {
        let mut interner = StringInterner::default();
        let left = interner.intern("max(orders.amount) > 1");
        let right = interner.intern("max(orders.amount) > 1");
        assert!(Arc::ptr_eq(&left, &right));
        assert_eq!(interner.unique_count(), 1);
    }
}
