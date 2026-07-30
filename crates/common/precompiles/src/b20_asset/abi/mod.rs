//! Versioned wire (ABI) surfaces for the asset-specific `IB20Asset` interface, one per hardfork
//! that moved them. The latest surface is named `IB20Asset` in its `vN` module and re-exported here
//! as both [`IB20Asset`] (canonical) and [`IB20AssetV2`]; older forks keep the same `IB20Asset` Rust
//! name inside their module so truncated-calldata revert bytes stay stable, re-exported as
//! [`IB20AssetV1`].
//!
//! Decoding is version-gated by [`B20Abi`](crate::B20Abi), selected per version by
//! [`AssetVersion::abi`](crate::AssetVersion).

use alloy_primitives::FixedBytes;

mod v1;
pub use v1::IB20Asset as IB20AssetV1;

mod v2;
pub use v2::{IB20Asset, IB20Asset as IB20AssetV2};

/// ERC-165 interface id (`IERC165`, `0x01ffc9a7`), the `supportsInterface(bytes4)` selector.
/// Advertised by `AssetV2` (ERC-8056 requires ERC-165); not itself an ERC-8056 id.
pub const ERC165_INTERFACE_ID: FixedBytes<4> = FixedBytes::new([0x01, 0xff, 0xc9, 0xa7]);

/// The ERC-8056 interface IDs advertised by `supportsInterface` from `AssetV2`.
pub const ERC8056_INTERFACE_IDS: [FixedBytes<4>; 3] = [
    FixedBytes::new([0xa6, 0x0b, 0xf1, 0x3d]),
    FixedBytes::new([0x4b, 0xd2, 0x76, 0x48]),
    FixedBytes::new([0xd8, 0x90, 0xfd, 0x71]),
];

impl IB20Asset::IB20AssetCalls {
    /// Returns the stable label for this decoded asset B-20 call.
    pub const fn as_label(&self) -> &'static str {
        match self {
            Self::OPERATOR_ROLE(_) => "precompile-b20-asset-OPERATOR_ROLE",
            Self::WAD_PRECISION(_) => "precompile-b20-asset-WAD_PRECISION",
            Self::announce(_) => "precompile-b20-asset-announce",
            Self::isAnnouncementIdUsed(_) => "precompile-b20-asset-isAnnouncementIdUsed",
            Self::multiplier(_) => "precompile-b20-asset-multiplier",
            Self::uiMultiplier(_) => "precompile-b20-asset-uiMultiplier",
            Self::newUIMultiplier(_) => "precompile-b20-asset-newUIMultiplier",
            Self::effectiveAt(_) => "precompile-b20-asset-effectiveAt",
            Self::toScaledBalance(_) => "precompile-b20-asset-toScaledBalance",
            Self::toRawBalance(_) => "precompile-b20-asset-toRawBalance",
            Self::scaledBalanceOf(_) => "precompile-b20-asset-scaledBalanceOf",
            Self::balanceOfUI(_) => "precompile-b20-asset-balanceOfUI",
            Self::totalSupplyUI(_) => "precompile-b20-asset-totalSupplyUI",
            Self::setUIMultiplier(_) => "precompile-b20-asset-setUIMultiplier",
            Self::cancelScheduledMultiplier(_) => "precompile-b20-asset-cancelScheduledMultiplier",
            Self::updateMultiplier(_) => "precompile-b20-asset-updateMultiplier",
            Self::supportsInterface(_) => "precompile-b20-asset-supportsInterface",
            Self::batchMint(_) => "precompile-b20-asset-batchMint",
            Self::extraMetadata(_) => "precompile-b20-asset-extraMetadata",
            Self::updateExtraMetadata(_) => "precompile-b20-asset-updateExtraMetadata",
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::FixedBytes;
    use alloy_sol_types::{SolCall, SolInterface};

    use crate::{ERC165_INTERFACE_ID, ERC8056_INTERFACE_IDS, IB20, IB20Asset};

    /// XORs the selectors of an interface's members into its ERC-165 interface id.
    fn xor(selectors: &[[u8; 4]]) -> FixedBytes<4> {
        let mut acc = [0u8; 4];
        for selector in selectors {
            for (a, s) in acc.iter_mut().zip(selector) {
                *a ^= *s;
            }
        }
        FixedBytes::new(acc)
    }

    #[test]
    fn erc8056_interface_ids_match_selectors() {
        // IERC165 = supportsInterface(bytes4); the ERC-165 id equals that selector by construction.
        assert_eq!(ERC165_INTERFACE_ID, xor(&[IB20Asset::supportsInterfaceCall::SELECTOR]));
        // IScaledUIAmount = uiMultiplier().
        assert_eq!(ERC8056_INTERFACE_IDS[0], xor(&[IB20Asset::uiMultiplierCall::SELECTOR]));
        // IScaledUIAmountNewUIMultiplier = newUIMultiplier() ^ effectiveAt().
        assert_eq!(
            ERC8056_INTERFACE_IDS[1],
            xor(&[IB20Asset::newUIMultiplierCall::SELECTOR, IB20Asset::effectiveAtCall::SELECTOR,])
        );
        // IScaledUIAmountBalances = balanceOfUI(address) ^ totalSupplyUI().
        assert_eq!(
            ERC8056_INTERFACE_IDS[2],
            xor(&[IB20Asset::balanceOfUICall::SELECTOR, IB20Asset::totalSupplyUICall::SELECTOR,])
        );
    }

    /// The Conversion extension id (`0x57854fc3`) must NOT be advertised.
    #[test]
    fn erc8056_conversion_extension_not_claimed() {
        assert!(!ERC8056_INTERFACE_IDS.contains(&FixedBytes::new([0x57, 0x85, 0x4f, 0xc3])));
    }

    #[test]
    fn asset_call_labels_are_stable() {
        assert_eq!(
            IB20Asset::IB20AssetCalls::updateExtraMetadata(IB20Asset::updateExtraMetadataCall {
                key: alloc::string::String::new(),
                value: alloc::string::String::new(),
            })
            .as_label(),
            "precompile-b20-asset-updateExtraMetadata"
        );
    }

    #[test]
    fn asset_and_inherited_call_selectors_are_disjoint() {
        for selector in IB20Asset::IB20AssetCalls::selectors() {
            assert!(
                !IB20::IB20Calls::valid_selector(selector),
                "asset selector {selector:?} overlaps with inherited IB20 selector"
            );
        }

        for selector in IB20::IB20Calls::selectors() {
            assert!(
                !IB20Asset::IB20AssetCalls::valid_selector(selector),
                "inherited IB20 selector {selector:?} overlaps with asset selector"
            );
        }
    }
}
