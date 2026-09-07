//! Block headers, sealed headers, and header mutation helpers.

mod sealed;
pub use sealed::{Header, SealedHeader};

mod header_mut;
pub use header_mut::HeaderMut;

#[cfg(any(test, feature = "test-utils", feature = "arbitrary"))]
pub mod test_utils;
