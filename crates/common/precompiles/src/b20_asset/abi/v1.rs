//! The asset B-20 wire surface frozen at Beryl, the fork where the asset variant activates.
//! A new wire surface goes in a new `abi/vN.rs`; see [`super`].
//!
//! [`IB20Asset`] defines only the asset-specific surface. All inherited selectors come from
//! [`crate::IB20`] defined in `common/abi.rs`.

use alloy_sol_types::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IB20Asset {
        // ── Errors ───────────────────────────────────────────────────────────

        /// `id` has previously been consumed by `announce`. Each id may be used at most once.
        error AnnouncementIdAlreadyUsed(string id);

        /// `updateExtraMetadata` was called with an empty metadata key.
        error InvalidMetadataKey();

        /// A multiplier setter (`updateMultiplier`) was called with a multiplier of zero or above
        /// the `type(uint128).max` overflow guard.
        error InvalidMultiplier();

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

        /// Emitted by `updateMultiplier` (V1, Beryl). The scheduled-multiplier version `AssetV2`
        /// emits `UIMultiplierUpdated` instead.
        event MultiplierUpdated(uint256 multiplier);

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

        /// Converts a raw balance to its scaled view: `rawBalance * multiplier / WAD_PRECISION`.
        function toScaledBalance(uint256 rawBalance) external view returns (uint256);

        /// Converts a scaled balance back to its raw representation.
        function toRawBalance(uint256 scaledBalance) external view returns (uint256 rawBalance);

        /// Convenience: `toScaledBalance(balanceOf(account))`.
        function scaledBalanceOf(address account) external view returns (uint256);

        /// Instant failsafe: sets the current multiplier immediately.
        /// At `AssetV1` emits `MultiplierUpdated`, which was replaced in `AssetV2` by
        /// `UIMultiplierUpdated`.
        function updateMultiplier(uint256 newMultiplier) external;

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

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_sol_types::{SolCall, SolInterface};

    use super::IB20Asset;

    /// See [`super`] — the interface name reaches consensus data via `AbiDecodeFailed` on short
    /// calldata, so it is pinned here rather than left to a future rename.
    #[test]
    fn interface_name_is_frozen() {
        assert_eq!(IB20Asset::IB20AssetCalls::NAME, "IB20AssetCalls");
    }

    /// The exact selector set dialable at Beryl. Adding or removing one changes which calls
    /// historical blocks could make.
    #[test]
    fn selector_set_is_frozen() {
        let mut selectors: Vec<[u8; 4]> = IB20Asset::IB20AssetCalls::selectors().collect();
        selectors.sort_unstable();

        let mut expected: Vec<[u8; 4]> = alloc::vec![
            IB20Asset::OPERATOR_ROLECall::SELECTOR,
            IB20Asset::WAD_PRECISIONCall::SELECTOR,
            IB20Asset::announceCall::SELECTOR,
            IB20Asset::isAnnouncementIdUsedCall::SELECTOR,
            IB20Asset::multiplierCall::SELECTOR,
            IB20Asset::toScaledBalanceCall::SELECTOR,
            IB20Asset::toRawBalanceCall::SELECTOR,
            IB20Asset::scaledBalanceOfCall::SELECTOR,
            IB20Asset::updateMultiplierCall::SELECTOR,
            IB20Asset::batchMintCall::SELECTOR,
            IB20Asset::extraMetadataCall::SELECTOR,
            IB20Asset::updateExtraMetadataCall::SELECTOR,
        ];
        expected.sort_unstable();

        assert_eq!(selectors.len(), 12);
        assert_eq!(selectors, expected);
    }

    /// The 11 selectors introduced at Cobalt (`AssetV2`) — the ERC-8056 scheduled UI-multiplier
    /// surface, the Conversion-extension `toUIAmount` / `fromUIAmount` aliases, and the
    /// `MAX_UI_MULTIPLIER` getter — must be absent from Beryl's frozen surface. That absence is what
    /// rejects them as `UnknownFunctionSelector` at V1, replacing the hand-written fork gate in
    /// `route`. Kept in sync with the 11-selector delta pinned by `v1_selectors_are_a_subset_of_v2`.
    #[test]
    fn asset_surface_excludes_v2_only_selectors() {
        // The V2-only selectors are named against the canonical (V2) surface via the crate root.
        for selector in [
            crate::IB20Asset::uiMultiplierCall::SELECTOR,
            crate::IB20Asset::newUIMultiplierCall::SELECTOR,
            crate::IB20Asset::effectiveAtCall::SELECTOR,
            crate::IB20Asset::balanceOfUICall::SELECTOR,
            crate::IB20Asset::totalSupplyUICall::SELECTOR,
            crate::IB20Asset::updateUIMultiplierCall::SELECTOR,
            crate::IB20Asset::cancelUIMultiplierUpdateCall::SELECTOR,
            crate::IB20Asset::toUIAmountCall::SELECTOR,
            crate::IB20Asset::fromUIAmountCall::SELECTOR,
            crate::IB20Asset::MAX_UI_MULTIPLIERCall::SELECTOR,
            crate::IB20Asset::supportsInterfaceCall::SELECTOR,
        ] {
            assert!(
                !IB20Asset::IB20AssetCalls::valid_selector(selector),
                "Beryl surface must not declare V2-only selector {selector:?}"
            );
        }
    }
}
