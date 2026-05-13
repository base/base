//! Storable type system for EVM storage.
//!
//! Re-exports core traits from [`crate::provider`] and defines `HandlerCache`.

pub mod array;
pub mod bytes_like;
pub mod mapping;
pub mod primitives;
pub mod set;
pub mod slot;
pub mod vec;

use std::{cell::RefCell, collections::HashMap, hash::Hash};

pub use bytes_like::*;
pub use mapping::Mapping;
pub use set::{Set, SetHandler};
pub use slot::Slot;

pub use crate::provider::{
    FromWord, Handler, Layout, LayoutCtx, Packable, Storable, StorableType, StorageKey,
};

/// Cache for computed handlers with stable references.
///
/// Enables `Index` implementations on handlers by storing child handlers and
/// returning references that remain valid across insertions.
#[derive(Debug, Default)]
pub struct HandlerCache<K, H> {
    inner: RefCell<HashMap<K, Box<H>>>,
}

impl<K, H> HandlerCache<K, H> {
    /// Creates a new empty handler cache.
    pub fn new() -> Self {
        Self { inner: RefCell::new(HashMap::new()) }
    }
}

impl<K, H> Clone for HandlerCache<K, H> {
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl<K: Hash + Eq + Clone, H> HandlerCache<K, H> {
    /// Returns a reference to a lazily initialized handler for the given key.
    pub fn get_or_insert(&self, key: &K, f: impl FnOnce() -> H) -> &H {
        let mut cache = self.inner.borrow_mut();
        if let Some(boxed) = cache.get(key) {
            // SAFETY: Box provides stable heap address. Cache is append-only.
            return unsafe { &*(boxed.as_ref() as *const H) };
        }
        let boxed = cache.entry(key.clone()).or_insert_with(|| Box::new(f()));
        // SAFETY: Box provides stable heap address. Cache is append-only.
        unsafe { &*(boxed.as_ref() as *const H) }
    }

    /// Returns a mutable reference to a lazily initialized handler for the given key.
    pub fn get_or_insert_mut(&mut self, key: &K, f: impl FnOnce() -> H) -> &mut H {
        let mut cache = self.inner.borrow_mut();
        if let Some(boxed) = cache.get_mut(key) {
            // SAFETY: Box provides stable heap address. `&mut self` ensures exclusive access.
            return unsafe { &mut *(boxed.as_mut() as *mut H) };
        }
        let boxed = cache.entry(key.clone()).or_insert_with(|| Box::new(f()));
        // SAFETY: Box provides stable heap address. `&mut self` ensures exclusive access.
        unsafe { &mut *(boxed.as_mut() as *mut H) }
    }
}
