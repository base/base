//! Wire (ABI) surfaces for the asset B-20 precompile, one per hardfork that moved them.
//!
//! The latest surface is always named `IB20Asset` in its `vN` module, then re-exported here as
//! both [`IB20Asset`] (canonical) and `IB20AssetVN`. Older forks keep the same Rust name inside
//! their module so truncated-calldata revert bytes stay stable, and are re-exported as
//! [`IB20AssetV1`], [`IB20AssetV2`], etc.
//!
//! Only the asset-specific surface is versioned here. The inherited [`crate::IB20`] surface has
//! not moved across forks, so it stays shared; a fingerprint pin in `common/abi.rs` is the
//! tripwire that forces splitting it the first time it (or `PausableFeature`) grows.

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
    use alloc::vec::Vec;

    use alloy_primitives::{B256, FixedBytes, b256, keccak256};
    use alloy_sol_types::{SolCall, SolError, SolEvent, SolInterface};

    use super::{ERC165_INTERFACE_ID, ERC8056_INTERFACE_IDS, IB20Asset, IB20AssetV1};
    use crate::IB20;

    /// Absolute wire fingerprint for Beryl's surface. Catches both-sides drift that relative
    /// V1==V2 asserts miss (alloy Display / signature changes that move every copy together).
    const V1_ABI_FINGERPRINT: B256 =
        b256!("cdd0644c49fc7cc90ae0e7153ee2c92ab2b82ac4450a07be4095d68318d173f5");

    /// Absolute wire fingerprint for Cobalt's (canonical) surface.
    const V2_ABI_FINGERPRINT: B256 =
        b256!("93c921285631a963f969f6c6541689d116fc1046050ea82f889d6a5d833e8026");

    /// Keccak of sorted call selectors, then sorted event topic0s, then sorted error selectors,
    /// then an enum-count byte, then enum ordinals. Order is fixed so a single pin catches any
    /// wire-surface edit.
    ///
    /// `IB20Asset` declares no enum, so the count byte is `0` and there are no ordinals. The layout
    /// deliberately matches the shared `AbiFingerprint::compute` helper introduced in #4206 (called
    /// here as if with `enum_count = 0` and empty ordinals); once that lands these fingerprints
    /// should call it directly, and the matching layout means no re-bless is needed.
    fn abi_fingerprint(
        selectors: impl IntoIterator<Item = [u8; 4]>,
        event_hashes: impl IntoIterator<Item = B256>,
        error_selectors: impl IntoIterator<Item = [u8; 4]>,
    ) -> B256 {
        let mut selectors: Vec<[u8; 4]> = selectors.into_iter().collect();
        selectors.sort_unstable();

        let mut event_hashes: Vec<B256> = event_hashes.into_iter().collect();
        event_hashes.sort_unstable();

        let mut error_selectors: Vec<[u8; 4]> = error_selectors.into_iter().collect();
        error_selectors.sort_unstable();

        let mut buf = Vec::with_capacity(
            selectors.len() * 4 + event_hashes.len() * 32 + error_selectors.len() * 4 + 1,
        );
        for selector in &selectors {
            buf.extend_from_slice(selector);
        }
        for hash in &event_hashes {
            buf.extend_from_slice(hash.as_slice());
        }
        for selector in &error_selectors {
            buf.extend_from_slice(selector);
        }
        buf.push(0);
        keccak256(&buf)
    }

    fn v1_abi_fingerprint() -> B256 {
        abi_fingerprint(
            IB20AssetV1::IB20AssetCalls::selectors(),
            IB20AssetV1::IB20AssetEvents::SELECTORS.iter().copied().map(B256::new),
            IB20AssetV1::IB20AssetErrors::selectors(),
        )
    }

    fn v2_abi_fingerprint() -> B256 {
        abi_fingerprint(
            IB20Asset::IB20AssetCalls::selectors(),
            IB20Asset::IB20AssetEvents::SELECTORS.iter().copied().map(B256::new),
            IB20Asset::IB20AssetErrors::selectors(),
        )
    }

    #[test]
    fn v1_abi_fingerprint_is_pinned() {
        assert_eq!(v1_abi_fingerprint(), V1_ABI_FINGERPRINT);
    }

    #[test]
    fn v2_abi_fingerprint_is_pinned() {
        assert_eq!(v2_abi_fingerprint(), V2_ABI_FINGERPRINT);
    }

    /// Events and errors carried over from Beryl keep their topic0 / selector. A signature drift
    /// here would change the logs and revert data of ops that both surfaces share.
    #[test]
    fn shared_events_and_errors_keep_their_signatures() {
        assert_eq!(
            IB20AssetV1::MultiplierUpdated::SIGNATURE_HASH,
            IB20Asset::MultiplierUpdated::SIGNATURE_HASH
        );
        assert_eq!(
            IB20AssetV1::ExtraMetadataUpdated::SIGNATURE_HASH,
            IB20Asset::ExtraMetadataUpdated::SIGNATURE_HASH
        );
        assert_eq!(
            IB20AssetV1::Announcement::SIGNATURE_HASH,
            IB20Asset::Announcement::SIGNATURE_HASH
        );
        assert_eq!(
            IB20AssetV1::EndAnnouncement::SIGNATURE_HASH,
            IB20Asset::EndAnnouncement::SIGNATURE_HASH
        );

        assert_eq!(
            IB20AssetV1::AnnouncementIdAlreadyUsed::SELECTOR,
            IB20Asset::AnnouncementIdAlreadyUsed::SELECTOR
        );
        assert_eq!(
            IB20AssetV1::InvalidMetadataKey::SELECTOR,
            IB20Asset::InvalidMetadataKey::SELECTOR
        );
        assert_eq!(
            IB20AssetV1::InvalidMultiplier::SELECTOR,
            IB20Asset::InvalidMultiplier::SELECTOR
        );
        assert_eq!(IB20AssetV1::LengthMismatch::SELECTOR, IB20Asset::LengthMismatch::SELECTOR);
        assert_eq!(IB20AssetV1::EmptyBatch::SELECTOR, IB20Asset::EmptyBatch::SELECTOR);
        assert_eq!(
            IB20AssetV1::AnnouncementInProgress::SELECTOR,
            IB20Asset::AnnouncementInProgress::SELECTOR
        );
        assert_eq!(
            IB20AssetV1::InternalCallMalformed::SELECTOR,
            IB20Asset::InternalCallMalformed::SELECTOR
        );
        assert_eq!(
            IB20AssetV1::InternalCallFailed::SELECTOR,
            IB20Asset::InternalCallFailed::SELECTOR
        );
    }

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
