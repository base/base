//! The shared B-20 wire surface frozen at Cobalt. Also the canonical live surface, re-exported
//! unqualified by [`super`]. A new wire surface goes in a new `abi/vN.rs`; see [`super`].
//!
//! Being canonical, this module also owns the absolute fingerprints for every frozen common surface.
//!
//! # Frozen — do not edit
//!
//! The interface stays named `IB20`, because `SolInterface::NAME` reaches the wire through
//! `AbiDecodeFailed` on short calldata. `PausableFeature` ordinals are also consensus data:
//! `B20PausableFeature::mask` derives pause storage bits as `1 << (feature as u8)`.

use alloy_sol_types::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IB20 {
        enum PausableFeature {
            /// Transfer operations.
            TRANSFER,
            /// Mint operations.
            MINT,
            /// Burn operations.
            BURN,
            /// Seize operations (`seizeWithMemo`).
            SEIZE
        }

        // Errors

        /// ETH was attached to a call targeting a nonpayable token selector.
        error NonPayable();

        error AccessControlUnauthorizedAccount(address account, bytes32 neededRole);
        error Unauthorized();
        error ContractPaused(PausableFeature feature);
        error InsufficientAllowance(address spender, uint256 allowance, uint256 needed);
        error InsufficientBalance(address sender, uint256 balance, uint256 needed);
        error InvalidSender(address sender);
        error InvalidReceiver(address receiver);
        error InvalidApprover(address approver);
        error InvalidSpender(address spender);
        error InvalidAmount();
        error EmptyFeatureSet();
        error InvalidSupplyCap(uint256 currentSupply, uint256 proposedCap);
        error SupplyCapExceeded(uint256 cap, uint256 attempted);
        error PolicyForbids(bytes32 policyScope, uint64 policyId);
        error PolicyNotFound(uint64 policyId);
        error UnsupportedPolicyType(bytes32 policyScope);
        error AccountNotBlocked(address account);
        error AccountNotSeizable(address account);
        error ExpiredSignature(uint256 deadline);
        error InvalidSigner(address signer, address owner);
        error LastAdminCannotRenounce();
        error NotSoleAdmin();
        error AccessControlBadConfirmation();

        // Events
        event Transfer(address indexed from, address indexed to, uint256 amount);
        event Approval(address indexed owner, address indexed spender, uint256 amount);
        event Memo(address indexed caller, bytes32 indexed memo);
        event BurnedBlocked(address indexed caller, address indexed from, uint256 amount);
        event Seized(address indexed caller, address indexed from, address indexed to, uint256 amount);
        event RoleGranted(bytes32 indexed role, address indexed account, address indexed sender);
        event RoleRevoked(bytes32 indexed role, address indexed account, address indexed sender);
        event RoleAdminChanged(bytes32 indexed role, bytes32 indexed previousAdminRole, bytes32 indexed newAdminRole);
        event LastAdminRenounced(address indexed previousAdmin);
        event Paused(address indexed updater, PausableFeature[] features);
        event Unpaused(address indexed updater, PausableFeature[] features);
        event PolicyUpdated(bytes32 indexed policyScope, uint64 oldPolicyId, uint64 newPolicyId);
        event SupplyCapUpdated(address indexed updater, uint256 oldSupplyCap, uint256 newSupplyCap);
        event ContractURIUpdated();
        event NameUpdated(address indexed updater, string newName);
        event SymbolUpdated(address indexed updater, string newSymbol);
        event EIP712DomainChanged();

        // Role identifiers
        function DEFAULT_ADMIN_ROLE() external view returns (bytes32);
        function MINT_ROLE() external view returns (bytes32);
        function BURN_ROLE() external view returns (bytes32);
        function BURN_BLOCKED_ROLE() external view returns (bytes32);
        function SEIZE_ROLE() external view returns (bytes32);
        function PAUSE_ROLE() external view returns (bytes32);
        function UNPAUSE_ROLE() external view returns (bytes32);
        function METADATA_ROLE() external view returns (bytes32);

        // Policy type identifiers
        function TRANSFER_SENDER_POLICY() external view returns (bytes32);
        function TRANSFER_RECEIVER_POLICY() external view returns (bytes32);
        function TRANSFER_EXECUTOR_POLICY() external view returns (bytes32);
        function MINT_RECEIVER_POLICY() external view returns (bytes32);
        function SEIZE_EXEMPT_POLICY() external view returns (bytes32);
        function SEIZE_RECEIVER_POLICY() external view returns (bytes32);

        // ERC-20
        function name() external view returns (string);
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
        function totalSupply() external view returns (uint256);
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function transfer(address to, uint256 amount) external returns (bool);
        function transferFrom(address from, address to, uint256 amount) external returns (bool);
        function approve(address spender, uint256 amount) external returns (bool);

        // Metadata updates
        function updateName(string calldata newName) external;
        function updateSymbol(string calldata newSymbol) external;

        // Memo transfer variants
        function transferWithMemo(address to, uint256 amount, bytes32 memo) external returns (bool);
        function transferFromWithMemo(address from, address to, uint256 amount, bytes32 memo) external returns (bool);

        // Mint / burn
        function mint(address to, uint256 amount) external;
        function mintWithMemo(address to, uint256 amount, bytes32 memo) external;
        function burn(uint256 amount) external;
        function burnWithMemo(uint256 amount, bytes32 memo) external;
        function burnBlocked(address from, uint256 amount) external;

        // Seize
        function seizeWithMemo(address from, address to, uint256 amount, bytes32 memo) external;

        // Roles
        function hasRole(bytes32 role, address account) external view returns (bool);
        function getRoleAdmin(bytes32 role) external view returns (bytes32);
        function grantRole(bytes32 role, address account) external;
        function revokeRole(bytes32 role, address account) external;
        function renounceRole(bytes32 role, address callerConfirmation) external;
        function renounceLastAdmin() external;
        function setRoleAdmin(bytes32 role, bytes32 newAdminRole) external;

        // Pause
        function pausedFeatures() external view returns (PausableFeature[] memory);
        function isPaused(PausableFeature feature) external view returns (bool);
        function pause(PausableFeature[] calldata features) external;
        function unpause(PausableFeature[] calldata features) external;

        // Policy
        function policyId(bytes32 policyScope) external view returns (uint64);
        function updatePolicy(bytes32 policyScope, uint64 newPolicyId) external;

        // Supply cap
        function supplyCap() external view returns (uint256);
        function updateSupplyCap(uint256 newSupplyCap) external;

        // Permit (EIP-2612 + ERC-5267)
        function DOMAIN_SEPARATOR() external view returns (bytes32);
        function nonces(address owner) external view returns (uint256);
        function permit(address owner, address spender, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s) external;
        function eip712Domain() external view returns (bytes1 fields, string memory name, string memory version, uint256 chainId, address verifyingContract, bytes32 salt, uint256[] memory extensions);

        // Contract URI (ERC-7572)
        function contractURI() external view returns (string);
        function updateContractURI(string calldata newURI) external;
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, b256};
    use alloy_sol_types::{SolEnum, SolInterface};

    use super::IB20;
    use crate::{AbiFingerprint, IB20V1};

    /// Absolute wire fingerprint for Beryl's surface. Catches both-sides drift that relative
    /// V1==V2 asserts miss (alloy Display / signature changes that move every copy together).
    ///
    /// This surface passes its `PausableFeature` ordinals to [`AbiFingerprint`], and they are
    /// load-bearing exactly as `B20Variant`'s are for the factory:
    /// `B20PausableFeature::mask` derives the pause storage bit as `1 << (feature as u8)`, so a
    /// `sol!` reorder of `TRANSFER`/`MINT`/`BURN` would silently remap which bit each feature
    /// toggles while leaving every selector, topic0, error selector and `COUNT` identical.
    const V1_ABI_FINGERPRINT: B256 =
        b256!("bcece6954307d847db07d6f723438cd1fa93ae40fe0c4c7ee5578779cb502661");

    /// Absolute wire fingerprint for Cobalt's (canonical) surface. Diverges from V1: Cobalt adds
    /// the seize surface (`seizeWithMemo`, the `SEIZE_ROLE`/`SEIZE_EXEMPT_POLICY`/`SEIZE_RECEIVER_POLICY`
    /// getters, the `Seized` event, the `AccountNotSeizable` error, and the `SEIZE` pause feature).
    const V2_ABI_FINGERPRINT: B256 =
        b256!("040ce4a6601295ef3caa0a4095a60295cf95726b6e783dcc40eb2dc42d5716eb");

    fn v1_abi_fingerprint() -> B256 {
        AbiFingerprint::compute(
            IB20V1::IB20Calls::selectors(),
            IB20V1::IB20Events::SELECTORS.iter().copied().map(B256::new),
            IB20V1::IB20Errors::selectors(),
            IB20V1::PausableFeature::COUNT,
            [
                IB20V1::PausableFeature::TRANSFER as u8,
                IB20V1::PausableFeature::MINT as u8,
                IB20V1::PausableFeature::BURN as u8,
            ],
        )
    }

    fn v2_abi_fingerprint() -> B256 {
        AbiFingerprint::compute(
            IB20::IB20Calls::selectors(),
            IB20::IB20Events::SELECTORS.iter().copied().map(B256::new),
            IB20::IB20Errors::selectors(),
            IB20::PausableFeature::COUNT,
            [
                IB20::PausableFeature::TRANSFER as u8,
                IB20::PausableFeature::MINT as u8,
                IB20::PausableFeature::BURN as u8,
                IB20::PausableFeature::SEIZE as u8,
            ],
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

    /// `SolInterface::NAME` lands in consensus data: the short-calldata branch of
    /// `abi_decode_validate` builds its error from it, and `AbiDecodeFailed` puts that string on
    /// the wire. Renaming the frozen interface would change historical revert payloads.
    #[test]
    fn interface_name_is_frozen() {
        assert_eq!(IB20::IB20Calls::NAME, "IB20Calls");
    }
}
