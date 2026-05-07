//! Type-keyed state registry for [`crate::runtime::Data<T>`] lookups.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

/// Type-keyed container of handler-visible state. Values are stored
/// as `Arc<dyn Any + Send + Sync>` so cloning is cheap and the
/// registry is safe to hand around an async boundary.
#[derive(Default)]
pub struct StateMap {
    inner: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl StateMap {
    /// Insert a value. A second insert of the same type overwrites.
    pub fn insert<T: Clone + Send + Sync + 'static>(&mut self, value: T) {
        self.inner.insert(TypeId::of::<T>(), Arc::new(value));
    }

    /// Look up a value by type. Returns `None` if no value of this
    /// type has been registered.
    pub fn get<T: Clone + Send + Sync + 'static>(&self) -> Option<T> {
        self.inner
            .get(&TypeId::of::<T>())
            .and_then(|a| a.downcast_ref::<T>())
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct Foo(u32);

    #[derive(Clone, Debug, PartialEq)]
    struct Bar(&'static str);

    #[test]
    fn insert_and_get_by_type() {
        let mut m = StateMap::default();
        m.insert(Foo(42));
        m.insert(Bar("hi"));
        assert_eq!(m.get::<Foo>(), Some(Foo(42)));
        assert_eq!(m.get::<Bar>(), Some(Bar("hi")));
        assert_eq!(m.get::<u32>(), None);
    }

    #[test]
    fn second_insert_overwrites() {
        let mut m = StateMap::default();
        m.insert(Foo(1));
        m.insert(Foo(2));
        assert_eq!(m.get::<Foo>(), Some(Foo(2)));
    }
}
