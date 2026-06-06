//! Storable type system for EVM storage.
//!
//! Re-exports core traits from [`crate::provider`] and defines `HandlerCache`.

mod array;
mod bytes_like;
mod mapping;
mod primitives;
mod set;
mod slot;
mod vec;

use alloc::{boxed::Box, collections::BTreeMap};
use core::cell::RefCell;

pub use array::ArrayHandler;
pub use bytes_like::BytesLikeHandler;
pub use mapping::{Mapping, MappingHandler};
pub use set::{Set, SetHandler};
pub use slot::Slot;
pub use vec::VecHandler;

/// Cache for computed handlers with stable references.
///
/// Enables `Index` implementations on handlers by storing child handlers and
/// returning references that remain valid across insertions.
///
/// INVARIANT: Once an entry is pushed, it must never be removed or replaced.
/// `get_or_insert` returns references into heap-allocated handlers that would
/// dangle if entries were evicted.
#[derive(Debug, Default)]
pub struct HandlerCache<K, H> {
    inner: RefCell<BTreeMap<K, Box<H>>>,
}

impl<K, H> HandlerCache<K, H> {
    /// Creates a new empty handler cache.
    pub const fn new() -> Self {
        Self { inner: RefCell::new(BTreeMap::new()) }
    }
}

impl<K, H> Clone for HandlerCache<K, H> {
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl<K: Ord + Clone, H> HandlerCache<K, H> {
    /// Returns a reference to a lazily initialized handler for the given key.
    pub fn get_or_insert(&self, key: &K, f: impl FnOnce() -> H) -> &H {
        let mut cache = self.inner.borrow_mut();
        if let Some(boxed) = cache.get(key) {
            // SAFETY: The returned reference intentionally outlives this `RefMut` guard.
            // `Box` gives `H` a stable heap address, this cache never removes or replaces
            // entries, and later `BTreeMap` inserts may move the `Box` pointer value but
            // not the boxed `H` allocation.
            return unsafe { &*(boxed.as_ref() as *const H) };
        }
        cache.insert(key.clone(), Box::new(f()));
        let boxed = cache.get(key).expect("handler cache was just populated");
        // SAFETY: See the safety note above. The newly inserted handler is also stored in
        // an append-only entry whose boxed allocation remains stable after this borrow ends.
        unsafe { &*(boxed.as_ref() as *const H) }
    }

    /// Returns a mutable reference to a lazily initialized handler for the given key.
    pub fn get_or_insert_mut(&mut self, key: &K, f: impl FnOnce() -> H) -> &mut H {
        // Using get_mut() requires &mut self (exclusive access) — no borrow guard needed.
        let cache = self.inner.get_mut();
        if !cache.contains_key(key) {
            cache.insert(key.clone(), Box::new(f()));
        }
        cache.get_mut(key).expect("handler cache was just populated").as_mut()
    }
}
