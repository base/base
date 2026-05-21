//! ABI definitions for the security B-20 variant.
//!
//! [`IB20Security`] defines only the security-specific surface.
//! All inherited selectors come from [`crate::IB20`] defined in `b20/abi.rs`.

use alloy_sol_types::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IB20Security {
        // ── Errors ───────────────────────────────────────────────────────────

        /// `id` has previously been consumed by `announce`. Each id may be used at most once.
        error AnnouncementIdAlreadyUsed(string id);

        /// `updateSecurityIdentifier` was called with an empty `identifierType`.
        error InvalidIdentifierType();

        /// A batched function was called with parallel arrays of differing lengths.
        error LengthMismatch(uint256 leftLen, uint256 rightLen);

        /// A batched function was called with empty arrays.
        error EmptyBatch();

        /// `redeem`/`redeemWithMemo` was called with a share count below the floor, or zero.
        error BelowMinimumRedeemable(uint256 shares, uint256 minimum);

        /// An `internalCalls` entry tried to invoke `announce` itself.
        error AnnouncementInProgress();

        /// An `internalCalls` entry was shorter than four bytes.
        error InternalCallMalformed(bytes call);

        /// An `internalCalls` entry reverted during its inner dispatch.
        error InternalCallFailed(bytes call);

        // ── Events ───────────────────────────────────────────────────────────

        /// Emitted by `redeem`/`redeemWithMemo`. Includes the active share ratio at redemption time.
        event Redeemed(address indexed from, uint256 amt, uint256 sharesToTokensRatio);

        /// Emitted by `updateMinimumRedeemable`.
        event MinimumRedeemableUpdated(uint256 newMinimumRedeemable);

        /// Emitted by `updateShareRatio`.
        event ShareRatioUpdated(uint256 sharesToTokensRatio);

        /// Emitted by `updateSecurityIdentifier`. Empty `value` indicates removal.
        event SecurityIdentifierUpdated(string identifierType, string value);

        /// Emitted at the start of `announce`. Indexers join with `EndAnnouncement` via `id`.
        event Announcement(address indexed caller, string id, string description, string uri);

        /// Emitted at the end of `announce` after all `internalCalls` have executed.
        event EndAnnouncement(string id);

        // ── Role / precision identifiers ─────────────────────────────────────

        /// `keccak256("SECURITY_OPERATOR_ROLE")` — required for `announce`, `updateShareRatio`, `updateSecurityIdentifier`.
        function SECURITY_OPERATOR_ROLE() external view returns (bytes32);

        /// `keccak256("BURN_FROM_ROLE")` — required for `batchBurn`.
        function BURN_FROM_ROLE() external view returns (bytes32);

        /// Fixed-point precision for `sharesToTokensRatio`: `1e18` (one WAD).
        function WAD_PRECISION() external view returns (uint256);

        /// `keccak256("REDEEMER_SENDER_POLICY")` — consulted on `redeem`/`redeemWithMemo`.
        function REDEEMER_SENDER_POLICY() external view returns (bytes32);

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

        // ── Share ratio ───────────────────────────────────────────────────────

        /// The current share-to-tokens ratio, scaled to `WAD_PRECISION`.
        function sharesToTokensRatio() external view returns (uint256);

        /// Converts `balance` tokens to shares: `balance * sharesToTokensRatio / WAD_PRECISION`.
        function toShares(uint256 balance) external view returns (uint256);

        /// Convenience: `toShares(balanceOf(account))`.
        function sharesOf(address account) external view returns (uint256);

        /// Sets a new share ratio. Holder balances are not rewritten; share count derives at read time.
        function updateShareRatio(uint256 newSharesToTokensRatio) external;

        // ── Batched issuance and clawback ────────────────────────────────────

        /// Mints `amounts[i]` to `recipients[i]`. Requires `MINT_ROLE`. All-or-nothing.
        function batchMint(address[] calldata recipients, uint256[] calldata amounts) external;

        /// Burns `amounts[i]` from `accounts[i]`. Requires `BURN_FROM_ROLE`. All-or-nothing.
        function batchBurn(address[] calldata accounts, uint256[] calldata amounts) external;

        // ── Redemption ────────────────────────────────────────────────────────

        /// Burns `amount` from caller with a share-based minimum floor check.
        function redeem(uint256 amount) external;

        /// Same as `redeem`, followed by a `Memo` event.
        function redeemWithMemo(uint256 amount, bytes32 memo) external;

        /// Sets the minimum-redeemable threshold in shares. Requires `DEFAULT_ADMIN_ROLE`.
        function updateMinimumRedeemable(uint256 newMinimumRedeemable) external;

        // ── Security identifiers ─────────────────────────────────────────────

        /// Returns the value of the named identifier (e.g. ISIN, CUSIP). Empty string if not set.
        function securityIdentifier(string calldata identifierType) external view returns (string);

        /// Sets, updates, or removes a security identifier. Empty `value` removes the entry.
        function updateSecurityIdentifier(
            string calldata identifierType,
            string calldata value
        ) external;
    }
}
