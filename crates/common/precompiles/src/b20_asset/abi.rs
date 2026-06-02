//! ABI definitions for the asset B-20 variant.
//!
//! [`IB20Asset`] defines only the asset-specific surface.
//! All inherited selectors come from [`crate::IB20`] defined in `b20/abi.rs`.

use alloy_sol_types::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IB20Asset {
        // ── Errors ───────────────────────────────────────────────────────────

        /// `id` has previously been consumed by `announce`. Each id may be used at most once.
        error AnnouncementIdAlreadyUsed(string id);

        /// `updateExtraMetadata` was called with an empty `identifierType`.
        error InvalidIdentifierType();

        /// A batched function was called with parallel arrays of differing lengths.
        error LengthMismatch(uint256 leftLen, uint256 rightLen);

        /// A batched function was called with empty arrays.
        error EmptyBatch();

        /// `redeem`/`redeemWithMemo` was called with a scaled amount below the floor, or zero.
        error BelowMinimumRedeemable(uint256 scaledAmount, uint256 minimum);

        /// An `internalCalls` entry tried to invoke `announce` itself.
        error AnnouncementInProgress();

        /// An `internalCalls` entry was shorter than four bytes.
        error InternalCallMalformed(bytes call);

        /// An `internalCalls` entry reverted during its inner dispatch.
        error InternalCallFailed(bytes call);

        // ── Events ───────────────────────────────────────────────────────────

        /// Emitted by `redeem`/`redeemWithMemo`. Includes the active multiplier at redemption time.
        event Redeemed(address indexed from, uint256 amt, uint256 multiplier);

        /// Emitted by `updateMinimumRedeemable`.
        event MinimumRedeemableUpdated(address indexed caller, uint256 newMinimumRedeemable);

        /// Emitted by `updateMultiplier`.
        event MultiplierUpdated(uint256 multiplier);

        /// Emitted by `updateExtraMetadata`. Empty `value` indicates removal.
        event ExtraMetadataUpdated(string identifierType, string value);

        /// Emitted at the start of `announce`. Indexers join with `EndAnnouncement` via `id`.
        event Announcement(address indexed caller, string id, string description, string uri);

        /// Emitted at the end of `announce` after all `internalCalls` have executed.
        event EndAnnouncement(string id);

        // ── Role / precision identifiers ─────────────────────────────────────

        /// `keccak256("OPERATOR_ROLE")` — required for `announce`, `updateMultiplier`, `updateExtraMetadata`.
        function OPERATOR_ROLE() external view returns (bytes32);

        /// Fixed-point precision for `multiplier`: `1e18` (one WAD).
        function WAD_PRECISION() external view returns (uint256);

        /// `keccak256("REDEEM_SENDER_POLICY")` — consulted on `redeem`/`redeemWithMemo`.
        function REDEEM_SENDER_POLICY() external view returns (bytes32);

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

        /// Convenience: `toScaledBalance(balanceOf(account))`.
        function scaledBalanceOf(address account) external view returns (uint256);

        /// Sets a new multiplier. Holder balances are not rewritten; scaled balances derive at read time.
        function updateMultiplier(uint256 newMultiplier) external;

        // ── Batched issuance and clawback ────────────────────────────────────

        /// Mints `amounts[i]` to `recipients[i]`. Requires `MINT_ROLE`. All-or-nothing.
        function batchMint(address[] calldata recipients, uint256[] calldata amounts) external;

        // ── Redemption ────────────────────────────────────────────────────────

        /// Burns `amount` from caller with a multiplier-scaled minimum floor check.
        function redeem(uint256 amount) external;

        /// Same as `redeem`, followed by a `Memo` event.
        function redeemWithMemo(uint256 amount, bytes32 memo) external;

        /// Sets the minimum-redeemable threshold in scaled units. Requires `DEFAULT_ADMIN_ROLE`.
        function updateMinimumRedeemable(uint256 newMinimumRedeemable) external;

        /// Returns the minimum-redeemable threshold in scaled units.
        function minimumRedeemable() external view returns (uint256);

        // ── Security identifiers ─────────────────────────────────────────────

        /// Returns the value of the named identifier (e.g. ISIN, CUSIP). Empty string if not set.
        function extraMetadata(string calldata identifierType) external view returns (string);

        /// Sets, updates, or removes a security identifier. Empty `value` removes the entry.
        function updateExtraMetadata(
            string calldata identifierType,
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
            Self::REDEEM_SENDER_POLICY(_) => "precompile-b20-asset-REDEEM_SENDER_POLICY",
            Self::announce(_) => "precompile-b20-asset-announce",
            Self::isAnnouncementIdUsed(_) => "precompile-b20-asset-isAnnouncementIdUsed",
            Self::multiplier(_) => "precompile-b20-asset-multiplier",
            Self::toScaledBalance(_) => "precompile-b20-asset-toScaledBalance",
            Self::scaledBalanceOf(_) => "precompile-b20-asset-scaledBalanceOf",
            Self::updateMultiplier(_) => "precompile-b20-asset-updateMultiplier",
            Self::batchMint(_) => "precompile-b20-asset-batchMint",
            Self::redeem(_) => "precompile-b20-asset-redeem",
            Self::redeemWithMemo(_) => "precompile-b20-asset-redeemWithMemo",
            Self::updateMinimumRedeemable(_) => "precompile-b20-asset-updateMinimumRedeemable",
            Self::minimumRedeemable(_) => "precompile-b20-asset-minimumRedeemable",
            Self::extraMetadata(_) => "precompile-b20-asset-extraMetadata",
            Self::updateExtraMetadata(_) => "precompile-b20-asset-updateExtraMetadata",
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, b256, keccak256};
    use alloy_sol_types::{SolCall, SolEvent};

    use crate::IB20Asset;

    #[test]
    fn redeem_sender_policy_selector_matches_solidity_interface() {
        assert_eq!(IB20Asset::REDEEM_SENDER_POLICYCall::SELECTOR, [0x1c, 0x6f, 0x9d, 0x42]);
    }

    #[test]
    fn minimum_redeemable_updated_topic_matches_solidity_interface() {
        assert_eq!(
            IB20Asset::MinimumRedeemableUpdated::SIGNATURE_HASH,
            b256!("7fdd6ea6dad98bfcd2c5ec538e748a5e8ecc40d0fc824f55dfc7397fe78a183b")
        );
        assert_eq!(
            IB20Asset::MinimumRedeemableUpdated::SIGNATURE_HASH,
            keccak256("MinimumRedeemableUpdated(address,uint256)")
        );
    }

    #[test]
    fn security_call_labels_are_stable() {
        assert_eq!(
            IB20Asset::IB20AssetCalls::minimumRedeemable(IB20Asset::minimumRedeemableCall {},)
                .as_label(),
            "precompile-b20-asset-minimumRedeemable"
        );
        assert_eq!(
            IB20Asset::IB20AssetCalls::redeem(IB20Asset::redeemCall { amount: U256::ZERO })
                .as_label(),
            "precompile-b20-asset-redeem"
        );
    }
}
