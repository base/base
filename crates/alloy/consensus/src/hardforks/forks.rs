//! Contains all hardforks represented in the [Hardfork] type.

use crate::{Ecotone, Fjord, Hardfork};
use alloc::vec::Vec;

/// Optimism Hardforks
///
/// This type is used to encapsulate hardfork transactions.
/// It exposes methods that return hardfork upgrade transactions
/// as [alloy_primitives::Bytes].
///
/// # Example
///
/// Build ecotone hardfork upgrade transaction:
/// ```rust
/// use op_alloy_consensus::{Hardfork, Hardforks};
/// let ecotone_upgrade_tx = Hardforks::ECOTONE.txs();
/// assert_eq!(ecotone_upgrade_tx.collect::<Vec<_>>().len(), 6);
/// ```
///
/// Build fjord hardfork upgrade transactions:
/// ```rust
/// use op_alloy_consensus::{Hardfork, Hardforks};
/// let fjord_upgrade_txs = Hardforks::FJORD.txs();
/// assert_eq!(fjord_upgrade_txs.collect::<Vec<_>>().len(), 3);
/// ```
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct Hardforks;

impl Hardforks {
    /// The ecotone hardfork upgrade transactions.
    pub const ECOTONE: Ecotone = Ecotone;

    /// The fjord hardfork upgrade transactions.
    pub const FJORD: Fjord = Fjord;

    /// Returns the ecotone hardfork upgrade transactions.
    #[deprecated]
    pub fn ecotone_txs() -> Vec<alloy_primitives::Bytes> {
        Ecotone.txs().collect()
    }

    /// Returns the fjord hardfork upgrade transactions.
    #[deprecated]
    pub fn fjord_txs() -> Vec<alloy_primitives::Bytes> {
        Fjord.txs().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hardforks() {
        let ecotone_upgrade_tx = Hardforks::ECOTONE.txs();
        assert_eq!(ecotone_upgrade_tx.collect::<Vec<_>>().len(), 6);

        let fjord_upgrade_txs = Hardforks::FJORD.txs();
        assert_eq!(fjord_upgrade_txs.collect::<Vec<_>>().len(), 3);
    }
}
