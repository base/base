//! Block headers, sealed headers, and header mutation helpers.

pub use base_common_types_chain::{Header, SealedHeader};

#[cfg(any(test, feature = "test-utils", feature = "arbitrary"))]
pub mod test_utils;
