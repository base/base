//! ABI definitions for the asset B-20 variant.
//!
//! [`IB20Asset`] defines only the asset-specific surface.
//! All inherited selectors come from [`crate::IB20`] defined in `b20/abi.rs`.

use alloy_primitives::FixedBytes;
use alloy_sol_types::sol;

/// ERC-165 interface id (`IERC165`, `0x01ffc9a7`), the `supportsInterface(bytes4)` selector.
/// Advertised by `AssetV2` (ERC-8056 requires ERC-165); not itself an ERC-8056 id.
pub const ERC165_INTERFACE_ID: FixedBytes<4> = FixedBytes::new([0x01, 0xff, 0xc9, 0xa7]);

/// The ERC-8056 interface IDs advertised by `supportsInterface` from `AssetV2`.
pub const ERC8056_INTERFACE_IDS: [FixedBytes<4>; 3] = [
    FixedBytes::new([0xa6, 0x0b, 0xf1, 0x3d]),
    FixedBytes::new([0x4b, 0xd2, 0x76, 0x48]),
    FixedBytes::new([0xd8, 0x90, 0xfd, 0x71]),
];

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IB20Asset {
        // ── Errors ───────────────────────────────────────────────────────────

        /// `id` has previously been consumed by `announce`. Each id may be used at most once.
        error AnnouncementIdAlreadyUsed(string id);

        /// `updateExtraMetadata` was called with an empty metadata key.
        error InvalidMetadataKey();

        /// A multiplier setter (`setUIMultiplier` / `updateMultiplier`) was called with a
        /// multiplier of zero or above the `type(uint128).max` overflow guard.
        error InvalidMultiplier();

        /// [V2] `setUIMultiplier` was called with an `effectiveAt` that is not in the future
        error EffectiveAtInPast(uint256 effectiveAt);

        /// [V2] `setUIMultiplier` was called with an `effectiveAt` beyond the `uint64` field range
        error EffectiveAtTooFar(uint256 effectiveAt);

        /// [V2] `setUIMultiplier` was called while a live pending update already exists.
        error ScheduleOverlap(uint256 pendingEffectiveAt);

        /// [V2] `cancelScheduledMultiplier` was called when there is no live pending update.
        error NoScheduledMultiplier();

        /// A batched function was called with parallel arrays of differing lengths.
        error LengthMismatch(uint256 leftLen, uint256 rightLen);

        /// A batched function was called with empty arrays.
        error EmptyBatch();

        /// An `internalCalls` entry tried to invoke `announce` itself.
        error AnnouncementInProgress();

        /// An `internalCalls` entry was shorter than four bytes.
        error InternalCallMalformed(bytes call);

        /// An `internalCalls` entry reverted during its inner dispatch.
        error InternalCallFailed(bytes call);

        // ── Events ───────────────────────────────────────────────────────────

        /// Emitted by `updateMultiplier` (V1, Beryl). Retained for the `AssetV1` version; the
        /// scheduled-multiplier version `AssetV2` emits `UIMultiplierUpdated` instead.
        event MultiplierUpdated(uint256 multiplier);

        /// [V2] ERC-8056; emitted by `setUIMultiplier` and `updateMultiplier`.
        event UIMultiplierUpdated(uint256 oldMultiplier, uint256 newMultiplier, uint256 effectiveAtTimestamp);

        /// [V2] Emitted by `cancelScheduledMultiplier`, and by `updateMultiplier` when it clears
        /// a live pending update.
        event MultiplierUpdateCancelled(uint256 cancelledMultiplier, uint256 cancelledEffectiveAt);

        /// Emitted by `updateExtraMetadata`. Empty `value` indicates removal.
        event ExtraMetadataUpdated(string key, string value);

        /// Emitted at the start of `announce`. Indexers join with `EndAnnouncement` via `id`.
        event Announcement(address indexed caller, string id, string description, string uri);

        /// Emitted at the end of `announce` after all `internalCalls` have executed.
        event EndAnnouncement(string id);

        // ── Role / precision identifiers ─────────────────────────────────────

        /// `keccak256("OPERATOR_ROLE")` — required for `announce` and `updateMultiplier`.
        function OPERATOR_ROLE() external view returns (bytes32);

        /// Fixed-point precision for `multiplier`: `1e18` (one WAD).
        function WAD_PRECISION() external view returns (uint256);


        // ── Announcements ────────────────────────────────────────────────────

        /// Posts a holder-impacting announcement and atomically executes `internalCalls`.
        function announce(
            bytes[] calldata internalCalls,
            string calldata id,
            string calldata description,
            string calldata uri
        ) external;

        /// Returns true if `id` has been consumed by `announce`.
        function isAnnouncementIdUsed(string calldata id) external view returns (bool);

        // ── Multiplier ────────────────────────────────────────────────────────

        /// The current multiplier, scaled to `WAD_PRECISION`.
        function multiplier() external view returns (uint256);

        /// [V2] ERC-8056 alias of `multiplier()`.
        function uiMultiplier() external view returns (uint256);

        /// [V2] ERC-8056: the multiplier scheduled to take effect, or the current multiplier when
        /// no scheduled update exists.
        function newUIMultiplier() external view returns (uint256);

        /// [V2] ERC-8056: the timestamp at which a scheduled multiplier becomes effective.
        function effectiveAt() external view returns (uint256);

        /// Converts a raw balance to its scaled view: `rawBalance * multiplier / WAD_PRECISION`.
        function toScaledBalance(uint256 rawBalance) external view returns (uint256);

        /// Converts a scaled balance back to its raw representation.
        function toRawBalance(uint256 scaledBalance) external view returns (uint256 rawBalance);

        /// Convenience: `toScaledBalance(balanceOf(account))`.
        function scaledBalanceOf(address account) external view returns (uint256);

        /// [V2] ERC-8056 Balances extension: alias of `scaledBalanceOf`.
        function balanceOfUI(address account) external view returns (uint256);

        /// [V2] ERC-8056 Balances extension: `totalSupply() * multiplier() / WAD_PRECISION`.
        function totalSupplyUI() external view returns (uint256);

        /// [V2] Schedules a single multiplier update effective at `effectiveAt`.
        /// The standard corporate-action path; requires `OPERATOR_ROLE`.
        function setUIMultiplier(uint256 newMultiplier, uint256 effectiveAt) external;

        /// [V2] Cancels the single live pending update, restoring the no-pending state.
        function cancelScheduledMultiplier() external;

        /// Instant failsafe: sets the current multiplier immediately and clears any pending.
        /// At `AssetV1` emits `MultiplierUpdated` which was replaced in `AssetV2` by `UIMultiplierUpdated`
        function updateMultiplier(uint256 newMultiplier) external;

        /// [V2] ERC-165 interface detection.
        function supportsInterface(bytes4 interfaceId) external view returns (bool);

        // ── Batched issuance and clawback ────────────────────────────────────

        /// Mints `amounts[i]` to `recipients[i]`. Requires `MINT_ROLE`. All-or-nothing.
        function batchMint(address[] calldata recipients, uint256[] calldata amounts) external;

        // ── Extra metadata ────────────────────────────────────────────────

        /// Returns the value of the named metadata entry (e.g. `"category"`, `"region"`). Empty string if not set.
        function extraMetadata(string calldata key) external view returns (string);

        /// Sets, updates, or removes an extra-metadata entry. Empty `value` removes the entry. Requires `METADATA_ROLE`.
        function updateExtraMetadata(
            string calldata key,
            string calldata value
        ) external;
    }
}

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
