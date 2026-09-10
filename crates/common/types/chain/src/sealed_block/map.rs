//! Concurrent maps using the chain primitive hasher.

pub use dashmap::{DashSet, Entry, mapref};
/// Concurrent map with the chain primitive hasher.
pub type DashMap<K, V, S = alloy_primitives::map::DefaultHashBuilder> = dashmap::DashMap<K, V, S>;
