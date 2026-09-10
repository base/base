//! Allocator selection for Base binaries.

#[cfg(all(feature = "jemalloc", unix))]
pub use tikv_jemalloc_sys;

/// Allocator used by Base binaries when jemalloc is enabled on Unix.
#[cfg(all(feature = "jemalloc", unix))]
pub type Allocator = tikv_jemallocator::Jemalloc;

/// System allocator used on other targets or without jemalloc.
#[cfg(not(all(feature = "jemalloc", unix)))]
pub type Allocator = std::alloc::System;

/// Creates the configured allocator.
pub const fn new_allocator() -> Allocator {
    Allocator {}
}
