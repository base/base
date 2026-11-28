//! The asset B-20 wire surface (`IB20Asset`), canonical since Cobalt, which added the ERC-8056
//! scheduled-multiplier selectors (see [`crate::B20Version`] for the selectors Beryl dials).
//! [`IB20Asset`] defines only the asset-specific surface; inherited selectors come from
//! [`crate::IB20`].

use alloy_primitives::FixedBytes;
use alloy_sol_types::sol;

/// ERC-165 interface id (`IERC165`, `0x01ffc9a7`), the `supportsInterface(bytes4)` selector.
/// Advertised by asset V2 (ERC-8056 requires ERC-165); not itself an ERC-8056 id.
pub const ERC165_INTERFACE_ID: FixedBytes<4> = FixedBytes::new([0x01, 0xff, 0xc9, 0xa7]);

/// The ERC-8056 interface IDs advertised by `supportsInterface` from asset V2:
/// `IScaledUIAmount`, `IScaledUIAmountNewUIMultiplier`, `IScaledUIAmountBalances`, and (added by the
/// interface review) the `IScaledUIAmountConversion` extension `0x57854fc3`.
pub const ERC8056_INTERFACE_IDS: [FixedBytes<4>; 4] = [
    FixedBytes::new([0xa6, 0x0b, 0xf1, 0x3d]),
    FixedBytes::new([0x4b, 0xd2, 0x76, 0x48]),
    FixedBytes::new([0xd8, 0x90, 0xfd, 0x71]),
    FixedBytes::new([0x57, 0x85, 0x4f, 0xc3]),
];

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IB20Asset {
        // ── Errors ───────────────────────────────────────────────────────────

        /// `id` has previously been consumed by `announce`. Each id may be used at most once.
        error AnnouncementIdAlreadyUsed(string id);

        /// `updateExtraMetadata` was called with an empty metadata key.
        error InvalidMetadataKey();

        /// A multiplier setter (`updateUIMultiplier` / `updateMultiplier`) was called with a
        /// multiplier of zero or above the `type(uint128).max` overflow guard.
        error InvalidMultiplier();

        /// [V2] `updateUIMultiplier` was called with an `effectiveAt` that is not in the future
        error EffectiveAtInPast(uint256 effectiveAt);

        /// [V2] `updateUIMultiplier` was called with an `effectiveAt` beyond the `uint64` field range
        error EffectiveAtTooFar(uint256 effectiveAt);

        /// [V2] `updateUIMultiplier` was called while a live pending update already exists.
        error UIMultiplierUpdateExists(uint256 effectiveAt);

        /// [V2] `cancelUIMultiplierUpdate` was called when there is no live pending update.
        error UIMultiplierUpdateDoesNotExist();

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

        /// Deprecated V1 event. asset V2's instant setter (`updateMultiplier`) emits this alongside
        /// `UIMultiplierUpdated` for backward compatibility with indexers on the legacy topic; the
        /// scheduled `updateUIMultiplier` emits only `UIMultiplierUpdated`.
        event MultiplierUpdated(uint256 multiplier);

        /// [V2] ERC-8056; emitted by `updateUIMultiplier` and `updateMultiplier`.
        event UIMultiplierUpdated(uint256 oldMultiplier, uint256 newMultiplier, uint256 effectiveAtTimestamp);

        /// [V2] Emitted by `cancelUIMultiplierUpdate`, and by `updateMultiplier` when it clears
        /// a live pending update.
        event UIMultiplierUpdateCancelled(uint256 cancelledMultiplier, uint256 cancelledEffectiveAt);

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

        /// [V2] The maximum UI multiplier the setters accept (`type(uint128).max`), the overflow
        /// guard. Exposed so callers can read the bound without triggering the revert path.
        function MAX_UI_MULTIPLIER() external view returns (uint256);


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
        /// Deprecated in favor of the ERC-8056 `toUIAmount`; retained (dialable) and kept in
        /// base-std's `IB20Asset` interface as a deprecated alias.
        function toScaledBalance(uint256 rawBalance) external view returns (uint256);

        /// Converts a scaled balance back to its raw representation.
        /// Deprecated in favor of the ERC-8056 `fromUIAmount`; retained (dialable) and kept in
        /// base-std's `IB20Asset` interface as a deprecated alias.
        function toRawBalance(uint256 scaledBalance) external view returns (uint256 rawBalance);

        /// [V2] ERC-8056 Conversion extension: raw -> UI amount, using the effective (lazily
        /// flipped) multiplier. Behaves identically to `toScaledBalance`.
        function toUIAmount(uint256 rawAmount) external view returns (uint256);

        /// [V2] ERC-8056 Conversion extension: UI -> raw amount. Behaves identically to `toRawBalance`.
        function fromUIAmount(uint256 uiAmount) external view returns (uint256);

        /// Convenience: `toScaledBalance(balanceOf(account))`.
        function scaledBalanceOf(address account) external view returns (uint256);

        /// [V2] ERC-8056 Balances extension: alias of `scaledBalanceOf`.
        function balanceOfUI(address account) external view returns (uint256);

        /// [V2] ERC-8056 Balances extension: `totalSupply() * multiplier() / WAD_PRECISION`.
        function totalSupplyUI() external view returns (uint256);

        /// [V2] Schedules a single UI-multiplier update effective at `effectiveAt` — the canonical
        /// corporate-action path (splits, reinvested dividends). Requires `OPERATOR_ROLE`.
        function updateUIMultiplier(uint256 newMultiplier, uint256 effectiveAt) external;

        /// [V2] Cancels the single live pending update, restoring the no-pending state.
        function cancelUIMultiplierUpdate() external;

        /// Instant failsafe: sets the current multiplier immediately and clears any pending.
        /// At asset V1 emits only `MultiplierUpdated`; asset V2 emits both `MultiplierUpdated` and
        /// `UIMultiplierUpdated`. Deprecated (retained dialable, and kept in base-std's `IB20Asset`
        /// interface as a deprecated function); the canonical setter is the scheduled
        /// `updateUIMultiplier`.
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
    use alloy_primitives::FixedBytes;
    use alloy_sol_types::{SolCall, SolInterface};

    use super::{ERC165_INTERFACE_ID, ERC8056_INTERFACE_IDS, IB20Asset};

    use crate::IB20;

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
        // IScaledUIAmountConversion = toUIAmount(uint256) ^ fromUIAmount(uint256).
        assert_eq!(
            ERC8056_INTERFACE_IDS[3],
            xor(&[IB20Asset::toUIAmountCall::SELECTOR, IB20Asset::fromUIAmountCall::SELECTOR,])
        );
    }

    /// The Conversion extension id (`0x57854fc3`) is advertised: the interface review reversed the
    /// prior opt-out so `toUIAmount` / `fromUIAmount` are exposed alongside the legacy conversions.
    #[test]
    fn erc8056_conversion_extension_is_claimed() {
        assert!(ERC8056_INTERFACE_IDS.contains(&FixedBytes::new([0x57, 0x85, 0x4f, 0xc3])));
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

    /// See [`super`] — the interface name reaches consensus data via `AbiDecodeFailed` on short
    /// calldata, so it is pinned here rather than left to a future rename.
    #[test]
    fn interface_name_is_frozen() {
        assert_eq!(IB20Asset::IB20AssetCalls::NAME, "IB20AssetCalls");
    }

    /// The exact selector set dialable at Cobalt (the 12 Beryl selectors plus the 8 ERC-8056
    /// scheduled-multiplier selectors plus the ERC-8056 Conversion-extension `toUIAmount` /
    /// `fromUIAmount` aliases and the `MAX_UI_MULTIPLIER` getter added by the interface review).
    /// Adding or removing one changes which calls historical blocks could make.
    #[test]
    fn selector_set_is_frozen() {
        let mut selectors: Vec<[u8; 4]> = IB20Asset::IB20AssetCalls::selectors().collect();
        selectors.sort_unstable();

        let mut expected: Vec<[u8; 4]> = alloc::vec![
            IB20Asset::OPERATOR_ROLECall::SELECTOR,
            IB20Asset::WAD_PRECISIONCall::SELECTOR,
            IB20Asset::MAX_UI_MULTIPLIERCall::SELECTOR,
            IB20Asset::announceCall::SELECTOR,
            IB20Asset::isAnnouncementIdUsedCall::SELECTOR,
            IB20Asset::multiplierCall::SELECTOR,
            IB20Asset::toScaledBalanceCall::SELECTOR,
            IB20Asset::toRawBalanceCall::SELECTOR,
            IB20Asset::toUIAmountCall::SELECTOR,
            IB20Asset::fromUIAmountCall::SELECTOR,
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
            IB20Asset::updateUIMultiplierCall::SELECTOR,
            IB20Asset::cancelUIMultiplierUpdateCall::SELECTOR,
            IB20Asset::supportsInterfaceCall::SELECTOR,
        ];
        expected.sort_unstable();

        assert_eq!(selectors.len(), 23);
        assert_eq!(selectors, expected);
    }
}
