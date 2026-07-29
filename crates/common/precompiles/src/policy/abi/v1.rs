//! The `PolicyRegistry` wire surface frozen at Beryl, the fork where the precompile activates.
//! A new wire surface goes in a new `abi/vN.rs`; see [`super`].

use alloy_sol_types::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IPolicyRegistry {
        enum PolicyType {
            /// Rejects only accounts explicitly added to the blocklist.
            /// An empty blocklist authorizes everyone.
            BLOCKLIST,
            /// Authorizes only accounts explicitly added to the allowlist.
            /// An empty allowlist rejects everyone.
            ALLOWLIST
        }

        /// ETH was attached to a call targeting a nonpayable policy registry selector.
        error NonPayable();

        error Unauthorized();
        error PolicyNotFound();
        error IncompatiblePolicyType();
        error ZeroAddress();
        error BatchSizeTooLarge(uint256 maxBatchSize);
        error NoPendingAdmin();

        event PolicyCreated(uint64 indexed policyId, address indexed creator, PolicyType policyType);
        event PolicyAdminStaged(uint64 indexed policyId, address indexed currentAdmin, address indexed pendingAdmin);
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
        function policyAdmin(uint64 policyId) external view returns (address);
        function pendingPolicyAdmin(uint64 policyId) external view returns (address);
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_sol_types::{SolCall, SolEnum, SolInterface};

    use super::IPolicyRegistry;

    /// The interface name reaches consensus data via `AbiDecodeFailed` on short calldata, so it is
    /// pinned here rather than left to a future rename. See the module docs.
    #[test]
    fn interface_name_is_frozen() {
        assert_eq!(IPolicyRegistry::IPolicyRegistryCalls::NAME, "IPolicyRegistryCalls");
    }

    /// Beryl's `PolicyType` has exactly two variants. A third would make composite discriminants
    /// decode at Beryl, changing the revert payload of historical blocks.
    #[test]
    fn policy_type_rejects_composite_discriminants() {
        assert_eq!(IPolicyRegistry::PolicyType::COUNT, 2);
        assert!(IPolicyRegistry::PolicyType::try_from(2u8).is_err());
        assert!(IPolicyRegistry::PolicyType::try_from(3u8).is_err());
    }

    /// The exact selector set dialable at Beryl. Adding or removing one changes which calls
    /// historical blocks could make.
    #[test]
    fn selector_set_is_frozen() {
        let mut selectors: Vec<[u8; 4]> =
            IPolicyRegistry::IPolicyRegistryCalls::selectors().collect();
        selectors.sort_unstable();

        let mut expected: Vec<[u8; 4]> = alloc::vec![
            IPolicyRegistry::createPolicyCall::SELECTOR,
            IPolicyRegistry::createPolicyWithAccountsCall::SELECTOR,
            IPolicyRegistry::stageUpdateAdminCall::SELECTOR,
            IPolicyRegistry::finalizeUpdateAdminCall::SELECTOR,
            IPolicyRegistry::renounceAdminCall::SELECTOR,
            IPolicyRegistry::updateAllowlistCall::SELECTOR,
            IPolicyRegistry::updateBlocklistCall::SELECTOR,
            IPolicyRegistry::isAuthorizedCall::SELECTOR,
            IPolicyRegistry::policyExistsCall::SELECTOR,
            IPolicyRegistry::policyAdminCall::SELECTOR,
            IPolicyRegistry::pendingPolicyAdminCall::SELECTOR,
        ];
        expected.sort_unstable();

        assert_eq!(selectors.len(), 11);
        assert_eq!(selectors, expected);
    }
}
