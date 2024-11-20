//! Utilities for creating hardforks.

use alloy_primitives::{hex, Address, Bytes};

/// UpgradeTo Function 4Byte Signature
pub(crate) const UPGRADE_TO_FUNC_BYTES_4: [u8; 4] = hex!("3659cfe6");

/// Turns the given address into calldata for the `upgradeTo` function.
pub(crate) fn upgrade_to_calldata(addr: Address) -> Bytes {
    let mut v = UPGRADE_TO_FUNC_BYTES_4.to_vec();
    v.extend_from_slice(addr.as_slice());
    v.into()
}
