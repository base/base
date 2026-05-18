//! The trait abstraction for an Upgrade.

use alloy_primitives::Bytes;

/// The trait abstraction for an Upgrade.
pub trait Upgrade {
    /// Returns the upgrade transactions as [`Bytes`].
    fn txs(&self) -> impl Iterator<Item = Bytes> + '_;
}
