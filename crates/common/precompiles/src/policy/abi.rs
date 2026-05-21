use alloy_sol_types::sol;
use base_precompile_storage::{BasePrecompileError, Result};

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IPolicyRegistry {
        enum PolicyType {
            /// Authorizes all accounts unconditionally.
            ALWAYS_ALLOW,
            /// Rejects all accounts unconditionally.
            ALWAYS_BLOCK,
            /// Authorizes only accounts explicitly added to the allowlist.
            ALLOWLIST,
            /// Rejects only accounts explicitly added to the blocklist.
            BLOCKLIST
        }

        error Unauthorized();
        error PolicyNotFound();
        error IncompatiblePolicyType();
        error InvalidPolicyType();
        error ZeroAddress();
        error NoPendingAdmin();
        error MalformedPolicyId(uint64 policyId);

        event PolicyCreated(uint64 indexed policyId, address indexed creator, PolicyType policyType);
        event PolicyAdminStaged(uint64 indexed policyId, address indexed previousAdmin, address indexed newAdmin);
        event PolicyAdminUpdated(uint64 indexed policyId, address indexed previousAdmin, address indexed newAdmin);
        event AllowlistUpdated(uint64 indexed policyId, address indexed updater, bool allowed, address[] accounts);
        event BlocklistUpdated(uint64 indexed policyId, address indexed updater, bool blocked, address[] accounts);

        function createPolicy(address admin, PolicyType policyType) external returns (uint64);
        function createPolicyWithAccounts(address admin, PolicyType policyType, address[] calldata accounts) external returns (uint64);
        /// Pass address(0) as newAdmin to clear a previously staged transfer without nominating a replacement.
        function stageUpdateAdmin(uint64 policyId, address newAdmin) external;
        function finalizeUpdateAdmin(uint64 policyId) external;
        function renounceAdmin(uint64 policyId) external;
        function updateAllowlist(uint64 policyId, bool allowed, address[] calldata accounts) external;
        function updateBlocklist(uint64 policyId, bool blocked, address[] calldata accounts) external;
        function isAuthorized(uint64 policyId, address account) external view returns (bool);
        function policyExists(uint64 policyId) external view returns (bool);
        function policyType(uint64 policyId) external view returns (PolicyType);
        function policyAdmin(uint64 policyId) external view returns (address);
        function pendingPolicyAdmin(uint64 policyId) external view returns (address);
    }
}

impl IPolicyRegistry::PolicyType {
    /// Returns the raw `u8` discriminant for ALLOWLIST or BLOCKLIST.
    /// Reverts with `InvalidPolicyType` for built-in sentinels (`ALWAYS_ALLOW`, `ALWAYS_BLOCK`).
    pub fn as_discriminant(self) -> Result<u8> {
        match self {
            Self::ALLOWLIST | Self::BLOCKLIST => Ok(self as u8),
            _ => Err(BasePrecompileError::revert(IPolicyRegistry::InvalidPolicyType {})),
        }
    }
}
