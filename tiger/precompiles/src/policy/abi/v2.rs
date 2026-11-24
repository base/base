//! The `PolicyRegistry` wire surface frozen at Cobalt, which added composite policies. Also the
//! canonical live surface, re-exported unqualified by [`super`].
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
            ALLOWLIST,
            /// Introduced in V2 (Cobalt). Composite gate: authorized if any child policy authorizes.
            UNION,
            /// Introduced in V2 (Cobalt). Composite gate: authorized only if every child policy authorizes.
            INTERSECT
        }

        /// ETH was attached to a call targeting a nonpayable policy registry selector.
        error NonPayable();

        error Unauthorized();
        error PolicyNotFound();
        error IncompatiblePolicyType();
        error ZeroAddress();
        error BatchSizeTooLarge(uint256 maxBatchSize);
        error NoPendingAdmin();

        /// Introduced in V2 (Cobalt). A composite policy was created or updated with a child-policy
        /// count outside the permitted range. A composite must reference between
        /// `MIN_COMPOSITE_CHILD_POLICIES` and `MAX_COMPOSITE_CHILD_POLICIES` simple policies, inclusive.
        error ChildPoliciesOutsideOfRange();
        /// Introduced in V2 (Cobalt). A composite child must be an existing ALLOWLIST or BLOCKLIST
        /// policy — never a built-in sentinel or another composite.
        error InvalidChildPolicy(uint64 childPolicyId);

        event PolicyCreated(uint64 indexed policyId, address indexed creator, PolicyType policyType);
        event PolicyAdminStaged(uint64 indexed policyId, address indexed currentAdmin, address indexed pendingAdmin);
        event PolicyAdminUpdated(uint64 indexed policyId, address indexed previousAdmin, address indexed newAdmin);
        event AllowlistUpdated(uint64 indexed policyId, address indexed updater, bool allowed, address[] accounts);
        event BlocklistUpdated(uint64 indexed policyId, address indexed updater, bool blocked, address[] accounts);
        /// Introduced in V2 (Cobalt). A composite policy's child set was set or replaced in full.
        /// Emitted on composite creation and on every subsequent update; carries the complete
        /// post-update set.
        event CompositePolicyUpdated(uint64 indexed policyId, address indexed updater, uint64[] childPolicyIds);

        function createPolicy(address admin, PolicyType policyType) external returns (uint64);
        function createPolicyWithAccounts(address admin, PolicyType policyType, address[] calldata accounts) external returns (uint64);
        /// Introduced in V2 (Cobalt). Creates a composite policy combining existing simple policies
        /// under a UNION or INTERSECT gate. Children must be simple policies; the child count must
        /// be in `[MIN_COMPOSITE_CHILD_POLICIES, MAX_COMPOSITE_CHILD_POLICIES]`.
        function createCompositePolicy(address admin, PolicyType policyType, uint64[] calldata childPolicyIds) external returns (uint64);
        /// Introduced in V2 (Cobalt). Replaces a composite policy's child set in full, re-validated
        /// exactly as at creation. The gate is fixed in the ID and cannot change.
        function updateComposite(uint64 policyId, uint64[] calldata childPolicyIds) external;
        /// Pass address(0) as newAdmin to clear a previously staged transfer without nominating a replacement.
        function stageUpdateAdmin(uint64 policyId, address newAdmin) external;
        function finalizeUpdateAdmin(uint64 policyId) external;
        function renounceAdmin(uint64 policyId) external;
        function updateAllowlist(uint64 policyId, bool allowed, address[] calldata accounts) external;
        function updateBlocklist(uint64 policyId, bool blocked, address[] calldata accounts) external;
        function isAuthorized(uint64 policyId, address account) external view returns (bool);
        /// Introduced in V2 (Cobalt). Minimum number of child policies a composite must
        /// reference, inclusive. Never reverts.
        function MIN_COMPOSITE_CHILD_POLICIES() external view returns (uint256);
        /// Introduced in V2 (Cobalt). Maximum number of child policies a composite may
        /// reference, inclusive. Never reverts.
        function MAX_COMPOSITE_CHILD_POLICIES() external view returns (uint256);
        function policyExists(uint64 policyId) external view returns (bool);
        function policyAdmin(uint64 policyId) external view returns (address);
        function pendingPolicyAdmin(uint64 policyId) external view returns (address);
        /// Introduced in V2 (Cobalt). Read function for composite policy child IDs.
        function compositePolicyChildIds(uint64 policyId) external view returns (uint64[] memory);
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_sol_types::{SolCall, SolEnum, SolInterface};

    use super::IPolicyRegistry;

    /// See [`super`] — the interface name reaches consensus data via `AbiDecodeFailed`.
    #[test]
    fn interface_name_is_frozen() {
        assert_eq!(IPolicyRegistry::IPolicyRegistryCalls::NAME, "IPolicyRegistryCalls");
    }

    #[test]
    fn policy_type_carries_the_composite_gates() {
        assert_eq!(IPolicyRegistry::PolicyType::COUNT, 4);
        assert_eq!(
            IPolicyRegistry::PolicyType::try_from(2u8),
            Ok(IPolicyRegistry::PolicyType::UNION)
        );
        assert_eq!(
            IPolicyRegistry::PolicyType::try_from(3u8),
            Ok(IPolicyRegistry::PolicyType::INTERSECT)
        );
    }

    /// The exact selector set dialable at Cobalt.
    #[test]
    fn selector_set_is_frozen() {
        let mut selectors: Vec<[u8; 4]> =
            IPolicyRegistry::IPolicyRegistryCalls::selectors().collect();
        selectors.sort_unstable();

        let mut expected: Vec<[u8; 4]> = alloc::vec![
            IPolicyRegistry::createPolicyCall::SELECTOR,
            IPolicyRegistry::createPolicyWithAccountsCall::SELECTOR,
            IPolicyRegistry::createCompositePolicyCall::SELECTOR,
            IPolicyRegistry::updateCompositeCall::SELECTOR,
            IPolicyRegistry::stageUpdateAdminCall::SELECTOR,
            IPolicyRegistry::finalizeUpdateAdminCall::SELECTOR,
            IPolicyRegistry::renounceAdminCall::SELECTOR,
            IPolicyRegistry::updateAllowlistCall::SELECTOR,
            IPolicyRegistry::updateBlocklistCall::SELECTOR,
            IPolicyRegistry::isAuthorizedCall::SELECTOR,
            IPolicyRegistry::MIN_COMPOSITE_CHILD_POLICIESCall::SELECTOR,
            IPolicyRegistry::MAX_COMPOSITE_CHILD_POLICIESCall::SELECTOR,
            IPolicyRegistry::policyExistsCall::SELECTOR,
            IPolicyRegistry::policyAdminCall::SELECTOR,
            IPolicyRegistry::pendingPolicyAdminCall::SELECTOR,
            IPolicyRegistry::compositePolicyChildIdsCall::SELECTOR,
        ];
        expected.sort_unstable();

        assert_eq!(selectors.len(), 16);
        assert_eq!(selectors, expected);
    }
}
