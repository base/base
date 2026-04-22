//! Utilities for creating upgrades.

use alloy_primitives::{Address, Bytes, hex};

/// Calldata builder for the `upgradeTo(address)` proxy upgrade function.
#[derive(Debug, Default, Clone, Copy)]
pub struct UpgradeCalldata;

impl UpgradeCalldata {
    /// `upgradeTo` function 4-byte selector.
    pub const SELECTOR: [u8; 4] = hex!("3659cfe6");

    /// Encodes calldata for `upgradeTo(addr)`.
    pub fn build(addr: Address) -> Bytes {
        let mut v = Self::SELECTOR.to_vec();
        v.extend_from_slice(addr.into_word().as_slice());
        v.into()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, keccak256};
    use rstest::rstest;

    use super::*;
    use crate::{Ecotone, Fjord, Isthmus};

    #[rstest]
    #[case("upgradeTo(address)", UpgradeCalldata::SELECTOR)]
    #[case("setEcotone()", Ecotone::ENABLE_ECOTONE_INPUT)]
    #[case("setFjord()", Fjord::SET_FJORD_METHOD_SIGNATURE)]
    #[case("setIsthmus()", Isthmus::ENABLE_ISTHMUS_INPUT)]
    fn test_selector_is_valid(#[case] sig: &str, #[case] expected: [u8; 4]) {
        assert_eq!(&keccak256(sig)[..4], expected);
    }

    #[test]
    fn test_upgrade_to_calldata_format() {
        let test_addr = Address::from([0x42; 20]);
        let calldata = UpgradeCalldata::build(test_addr);

        assert_eq!(calldata.len(), 36);
        assert_eq!(&calldata[..4], UpgradeCalldata::SELECTOR);
        assert_eq!(&calldata[4..36], test_addr.into_word().as_slice());
    }
}
