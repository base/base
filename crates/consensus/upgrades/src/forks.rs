//! Contains all upgrades represented in the [`crate::Upgrade`] type.

use crate::{Ecotone, Fjord, Isthmus, Jovian};

/// Base Upgrades
///
/// This type is used to encapsulate upgrade transactions.
/// It exposes methods that return upgrade transactions
/// as [`alloy_primitives::Bytes`].
///
/// # Example
///
/// Build ecotone upgrade transaction:
/// ```rust
/// use base_consensus_upgrades::{Upgrade, Upgrades};
/// let ecotone_upgrade_tx = Upgrades::ECOTONE.txs();
/// assert_eq!(ecotone_upgrade_tx.collect::<Vec<_>>().len(), 6);
/// ```
///
/// Build fjord upgrade transactions:
/// ```rust
/// use base_consensus_upgrades::{Upgrade, Upgrades};
/// let fjord_upgrade_txs = Upgrades::FJORD.txs();
/// assert_eq!(fjord_upgrade_txs.collect::<Vec<_>>().len(), 3);
/// ```
///
/// Build isthmus upgrade transaction:
/// ```rust
/// use base_consensus_upgrades::{Upgrade, Upgrades};
/// let isthmus_upgrade_tx = Upgrades::ISTHMUS.txs();
/// assert_eq!(isthmus_upgrade_tx.collect::<Vec<_>>().len(), 8);
/// ```
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct Upgrades;

impl Upgrades {
    /// The Ecotone upgrade transactions.
    pub const ECOTONE: Ecotone = Ecotone;

    /// The Fjord upgrade transactions.
    pub const FJORD: Fjord = Fjord;

    /// The Isthmus upgrade transactions.
    pub const ISTHMUS: Isthmus = Isthmus;

    /// The Jovian upgrade transactions.
    pub const JOVIAN: Jovian = Jovian;
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    use crate::Upgrade;

    #[test]
    fn test_upgrades() {
        let ecotone_upgrade_tx = Upgrades::ECOTONE.txs();
        assert_eq!(ecotone_upgrade_tx.collect::<Vec<_>>().len(), 6);

        let fjord_upgrade_txs = Upgrades::FJORD.txs();
        assert_eq!(fjord_upgrade_txs.collect::<Vec<_>>().len(), 3);

        let isthmus_upgrade_tx = Upgrades::ISTHMUS.txs();
        assert_eq!(isthmus_upgrade_tx.collect::<Vec<_>>().len(), 8);

        let jovian_upgrade_tx = Upgrades::JOVIAN.txs();
        assert_eq!(jovian_upgrade_tx.collect::<Vec<_>>().len(), 5);
    }
}
