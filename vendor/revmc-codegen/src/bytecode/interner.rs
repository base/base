use std::hash::{BuildHasher, Hash};

use indexmap::IndexSet;
use oxc_index::Idx;

/// A generic interner that deduplicates values and returns compact indices.
///
/// Backed by an [`IndexSet`] so that insertion is O(1) amortized and
/// index-to-value lookup is O(1).
pub(crate) struct Interner<I: Idx, T, S = alloy_primitives::map::DefaultHashBuilder> {
    set: IndexSet<T, S>,
    _marker: std::marker::PhantomData<fn() -> I>,
}

impl<I: Idx, T: Hash + Eq, S: BuildHasher + Default> Default for Interner<I, T, S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I: Idx, T: Hash + Eq, S: BuildHasher + Default> Interner<I, T, S> {
    pub(crate) fn new() -> Self {
        Self::with_capacity(0)
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            set: IndexSet::with_capacity_and_hasher(capacity, S::default()),
            _marker: std::marker::PhantomData,
        }
    }

    /// Interns a value, returning its deduplicated index.
    pub(crate) fn intern(&mut self, value: T) -> I {
        let (idx, _) = self.set.insert_full(value);
        I::from_usize(idx)
    }
}

impl<I: Idx, T, S> Interner<I, T, S> {
    /// Returns the value at the given index.
    #[inline]
    pub(crate) fn get(&self, idx: I) -> &T {
        &self.set[idx.index()]
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.set.len()
    }
}

impl<I: Idx, T, S> std::ops::Index<I> for Interner<I, T, S> {
    type Output = T;

    #[inline]
    fn index(&self, idx: I) -> &T {
        &self.set[idx.index()]
    }
}

impl<I: Idx, T: std::fmt::Debug, S> std::fmt::Debug for Interner<I, T, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.set.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    oxc_index::define_index_type! {
        struct TestIdx = u32;
    }

    #[test]
    fn deduplication() {
        let mut interner = Interner::<TestIdx, &str>::new();
        let a = interner.intern("hello");
        let b = interner.intern("world");
        let c = interner.intern("hello");
        assert_eq!(a, c);
        assert_ne!(a, b);
        assert_eq!(interner.len(), 2);
    }

    #[test]
    fn get_and_index() {
        let mut interner = Interner::<TestIdx, u64>::new();
        let idx = interner.intern(42);
        assert_eq!(*interner.get(idx), 42);
        assert_eq!(interner[idx], 42);
    }

    #[test]
    fn stable_indices() {
        let mut interner = Interner::<TestIdx, u64>::new();
        let indices: Vec<_> = (0..100).map(|i| interner.intern(i)).collect();
        // Re-interning produces the same indices.
        for i in 0..100u64 {
            assert_eq!(interner.intern(i), indices[i as usize]);
        }
        assert_eq!(interner.len(), 100);
    }

    #[test]
    fn with_capacity() {
        let mut interner = Interner::<TestIdx, &str>::with_capacity(16);
        let idx = interner.intern("test");
        assert_eq!(*interner.get(idx), "test");
    }
}
