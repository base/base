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

use alloc::{boxed::Box, vec::Vec};
use core::{cell::RefCell, hash::Hash};

pub use array::ArrayHandler;
pub use bytes_like::BytesLikeHandler;
pub use mapping::Mapping;
pub use set::{Set, SetHandler};
pub use slot::Slot;
pub use vec::VecHandler;

/// Cache for computed handlers with stable references.
///
/// Enables `Index` implementations on handlers by storing child handlers and
/// returning references that remain valid across insertions.
#[derive(Debug, Default)]
pub struct HandlerCache<K, H> {
    inner: RefCell<Vec<(K, Box<H>)>>,
}

impl<K, H> HandlerCache<K, H> {
    /// Creates a new empty handler cache.
    pub const fn new() -> Self {
        Self { inner: RefCell::new(Vec::new()) }
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
        if let Some((_, boxed)) = cache.iter().find(|(cached, _)| cached == key) {
            // SAFETY: Box provides stable heap address. Cache is append-only.
            return unsafe { &*(boxed.as_ref() as *const H) };
        }
        cache.push((key.clone(), Box::new(f())));
        let boxed = &cache.last().expect("handler cache was just populated").1;
        // SAFETY: Box provides stable heap address. Cache is append-only.
        unsafe { &*(boxed.as_ref() as *const H) }
    }

    /// Returns a mutable reference to a lazily initialized handler for the given key.
    pub fn get_or_insert_mut(&mut self, key: &K, f: impl FnOnce() -> H) -> &mut H {
        // Using get_mut() requires &mut self (exclusive access) — no borrow guard needed.
        let cache = self.inner.get_mut();
        if let Some(index) = cache.iter().position(|(cached, _)| cached == key) {
            return cache[index].1.as_mut();
        }
        cache.push((key.clone(), Box::new(f())));
        cache.last_mut().expect("handler cache was just populated").1.as_mut()
    }
}
