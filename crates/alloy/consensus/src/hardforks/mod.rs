//! OP Stack Hardfork Transaction Updates

use alloy_primitives::{address, Address};

mod fjord;
pub use fjord::{FJORD_GAS_PRICE_ORACLE, GAS_PRICE_ORACLE_FJORD_DEPLOYER, L1_INFO_DEPOSITER};

mod ecotone;
pub use ecotone::{EIP4788_FROM, GAS_PRICE_ORACLE_DEPLOYER, L1_BLOCK_DEPLOYER, NEW_L1_BLOCK};

/// The Gas Price Oracle Address
/// This is computed by using go-ethereum's `crypto.CreateAddress` function,
/// with the Gas Price Oracle Deployer Address and nonce 0.
pub const GAS_PRICE_ORACLE: Address = address!("b528d11cc114e026f138fe568744c6d45ce6da7a");

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
    /// UpgradeTo Function 4Byte Signature
    pub const UPGRADE_TO_FUNC_BYTES_4: [u8; 4] = alloy_primitives::hex!("3659cfe6");

    /// Turns the given address into calldata for the `upgradeTo` function.
    pub(crate) fn upgrade_to_calldata(addr: alloy_primitives::Address) -> alloy_primitives::Bytes {
        let mut v = Self::UPGRADE_TO_FUNC_BYTES_4.to_vec();
        v.extend_from_slice(addr.as_slice());
        alloy_primitives::Bytes::from(v)
    }
}
