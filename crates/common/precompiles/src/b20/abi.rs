//! ABI definition for the `IB20` interface.

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
            /// Reserved for future redeem operations; no current B-20 operation checks this flag.
            REDEEM
        }

        // Errors
        error AccessControlUnauthorizedAccount(address account, bytes32 neededRole);
        error Unauthorized();
        error ContractPaused(PausableFeature feature);
        error InsufficientAllowance(address spender, uint256 allowance, uint256 needed);
        error InsufficientBalance(address sender, uint256 balance, uint256 needed);
        error InvalidSender(address sender);
        error InvalidReceiver(address receiver);
        error InvalidApprover(address approver);
        error InvalidSpender(address spender);
        error EmptyFeatureSet();
        error InvalidSupplyCap(uint256 currentSupply, uint256 proposedCap);
        error SupplyCapExceeded(uint256 cap, uint256 attempted);
        error PolicyForbids(bytes32 policyType, uint64 policyId);
        error PolicyNotFound(uint64 policyId);
        error UnsupportedPolicyType(bytes32 policyType);
        error AccountNotBlocked(address account);
        error ExpiredSignature(uint256 deadline);
        error InvalidSigner(address signer, address owner);
        error Uninitialized();
        error LastAdminCannotRenounce();
        error NotSoleAdmin();
        error AccessControlBadConfirmation();

        // Events
        event Transfer(address indexed from, address indexed to, uint256 amount);
        event Approval(address indexed owner, address indexed spender, uint256 amount);
        event Memo(bytes32 indexed memo);
        event BurnedBlocked(address indexed caller, address indexed from, uint256 amount);
        event RoleGranted(bytes32 indexed role, address indexed account, address indexed sender);
        event RoleRevoked(bytes32 indexed role, address indexed account, address indexed sender);
        event RoleAdminChanged(bytes32 indexed role, bytes32 indexed previousAdminRole, bytes32 indexed newAdminRole);
        event LastAdminRenounced(address indexed previousAdmin);
        event Paused(address indexed updater, PausableFeature[] features);
        event Unpaused(address indexed updater, PausableFeature[] features);
        event PolicyUpdated(bytes32 indexed policyType, uint64 oldPolicyId, uint64 newPolicyId);
        event SupplyCapUpdated(address indexed updater, uint256 oldSupplyCap, uint256 newSupplyCap);
        event ContractURIUpdated();
        event NameUpdated(address indexed updater, string newName);
        event SymbolUpdated(address indexed updater, string newSymbol);

        // Role identifiers
        function DEFAULT_ADMIN_ROLE() external view returns (bytes32);
        function MINT_ROLE() external view returns (bytes32);
        function BURN_ROLE() external view returns (bytes32);
        function BURN_BLOCKED_ROLE() external view returns (bytes32);
        function PAUSE_ROLE() external view returns (bytes32);
        function UNPAUSE_ROLE() external view returns (bytes32);
        function METADATA_ROLE() external view returns (bytes32);

        // Policy type identifiers
        function TRANSFER_SENDER_POLICY() external view returns (bytes32);
        function TRANSFER_RECEIVER_POLICY() external view returns (bytes32);
        function TRANSFER_EXECUTOR_POLICY() external view returns (bytes32);
        function MINT_RECEIVER_POLICY() external view returns (bytes32);

        // ERC-20
        function name() external view returns (string);
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
        function totalSupply() external view returns (uint256);
        function minimumRedeemable() external view returns (uint256);
        function currency() external view returns (string);
        function securityIdentifier(string calldata identifierType) external view returns (string);
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function transfer(address to, uint256 amount) external returns (bool);
        function transferFrom(address from, address to, uint256 amount) external returns (bool);
        function approve(address spender, uint256 amount) external returns (bool);

        // Metadata updates
        function setName(string calldata newName) external;
        function setSymbol(string calldata newSymbol) external;

        // Memo transfer variants
        function transferWithMemo(address to, uint256 amount, bytes32 memo) external returns (bool);
        function transferFromWithMemo(address from, address to, uint256 amount, bytes32 memo) external returns (bool);

        // Mint / burn
        function mint(address to, uint256 amount) external;
        function mintWithMemo(address to, uint256 amount, bytes32 memo) external;
        function burn(uint256 amount) external;
        function burnWithMemo(uint256 amount, bytes32 memo) external;
        function burnBlocked(address from, uint256 amount) external;

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
        function policyId(bytes32 policyType) external view returns (uint64);
        function updatePolicy(bytes32 policyType, uint64 newPolicyId) external;

        // Supply cap
        function supplyCap() external view returns (uint256);
        function setSupplyCap(uint256 newSupplyCap) external;

        // Permit (EIP-2612 + ERC-5267)
        function DOMAIN_SEPARATOR() external view returns (bytes32);
        function nonces(address owner) external view returns (uint256);
        function permit(address owner, address spender, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s) external;
        function eip712Domain() external view returns (bytes1 fields, string memory name, string memory version, uint256 chainId, address verifyingContract, bytes32 salt, uint256[] memory extensions);

        // Contract URI (ERC-7572)
        function contractURI() external view returns (string);
        function setContractURI(string calldata newURI) external;
    }
}
