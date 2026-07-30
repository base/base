//! The asset B-20 wire surface frozen at Cobalt, which added the ERC-8056 scheduled-multiplier
//! selectors. Also the canonical live surface, re-exported unqualified by [`super`].
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

    /// The exact selector set dialable at Cobalt (the 12 Beryl selectors plus the 8 ERC-8056
    /// scheduled-multiplier selectors). Adding or removing one changes which calls historical
    /// blocks could make.
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
            IB20Asset::uiMultiplierCall::SELECTOR,
            IB20Asset::newUIMultiplierCall::SELECTOR,
            IB20Asset::effectiveAtCall::SELECTOR,
            IB20Asset::balanceOfUICall::SELECTOR,
            IB20Asset::totalSupplyUICall::SELECTOR,
            IB20Asset::setUIMultiplierCall::SELECTOR,
            IB20Asset::cancelScheduledMultiplierCall::SELECTOR,
            IB20Asset::supportsInterfaceCall::SELECTOR,
        ];
        expected.sort_unstable();

        assert_eq!(selectors.len(), 20);
        assert_eq!(selectors, expected);
    }
}
