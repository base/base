//! OP Stack Hardfork Transaction Updates

pub mod ecotone;
pub mod fjord;

/// UpgradeTo Function 4Byte Signature
pub(crate) const UPGRADE_TO_FUNC_BYTES_4: &[u8] = &[0x36, 0x59, 0xcf, 0xe6];

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
/// use op_alloy_consensus::Hardforks;
/// let ecotone_upgrade_tx = Hardforks::ecotone_txs();
/// assert_eq!(ecotone_upgrade_tx.len(), 6);
/// ```
///
/// Build fjord hardfork upgrade transactions:
/// ```rust
/// use op_alloy_consensus::Hardforks;
/// let fjord_upgrade_txs = Hardforks::fjord_txs();
/// assert_eq!(fjord_upgrade_txs.len(), 3);
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct Hardforks;

impl Hardforks {
    /// Turns the given address into calldata for the `upgradeTo` function.
    pub(crate) fn upgrade_to_calldata(addr: alloy_primitives::Address) -> alloy_primitives::Bytes {
        let mut v = UPGRADE_TO_FUNC_BYTES_4.to_vec();
        v.extend_from_slice(addr.as_slice());
        alloy_primitives::Bytes::from(v)
    }
}
