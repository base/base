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
        /// count outside the permitted `[min, max]` range.
        error ChildPoliciesOutsideOfRange(uint256 min, uint256 max);
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
        /// be in `[2, 4]`.
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
        function policyExists(uint64 policyId) external view returns (bool);
        function policyAdmin(uint64 policyId) external view returns (address);
        function pendingPolicyAdmin(uint64 policyId) external view returns (address);
    }
}

impl IPolicyRegistry::IPolicyRegistryCalls {
    /// Returns the stable metric label for this decoded policy-registry call.
    pub const fn as_label(&self) -> &'static str {
        match self {
            Self::createPolicy(_) => "policy.createPolicy",
            Self::createPolicyWithAccounts(_) => "policy.createPolicyWithAccounts",
            Self::createCompositePolicy(_) => "policy.createCompositePolicy",
            Self::updateComposite(_) => "policy.updateComposite",
            Self::stageUpdateAdmin(_) => "policy.stageUpdateAdmin",
            Self::finalizeUpdateAdmin(_) => "policy.finalizeUpdateAdmin",
            Self::renounceAdmin(_) => "policy.renounceAdmin",
            Self::updateAllowlist(_) => "policy.updateAllowlist",
            Self::updateBlocklist(_) => "policy.updateBlocklist",
            Self::isAuthorized(_) => "policy.isAuthorized",
            Self::policyExists(_) => "policy.policyExists",
            Self::policyAdmin(_) => "policy.policyAdmin",
            Self::pendingPolicyAdmin(_) => "policy.pendingPolicyAdmin",
        }
    }
}

impl IPolicyRegistry::PolicyType {
    /// Returns the raw `u8` discriminant for this policy type.
    pub const fn as_discriminant(self) -> u8 {
        self as u8
    }

    /// Returns whether this value is a supported *simple* policy type.
    ///
    /// Only `BLOCKLIST`/`ALLOWLIST` are simple types accepted by `createPolicy`; composite
    /// gates (`UNION`/`INTERSECT`) are created via `createCompositePolicy` and are not valid here.
    pub const fn is_valid(self) -> bool {
        matches!(self, Self::BLOCKLIST | Self::ALLOWLIST)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use alloy_sol_types::SolEnum;

    use super::IPolicyRegistry;

    #[test]
    fn simple_policy_types_are_valid_composites_are_not() {
        // Simple leaf types are valid for `createPolicy`.
        assert!(IPolicyRegistry::PolicyType::BLOCKLIST.is_valid());
        assert!(IPolicyRegistry::PolicyType::ALLOWLIST.is_valid());

        // Composite gates are created via `createCompositePolicy`, not `createPolicy`.
        assert!(!IPolicyRegistry::PolicyType::UNION.is_valid());
        assert!(!IPolicyRegistry::PolicyType::INTERSECT.is_valid());

        // Every generated discriminant still decodes to a variant.
        for discriminant in 0..IPolicyRegistry::PolicyType::COUNT {
            IPolicyRegistry::PolicyType::try_from(discriminant as u8)
                .expect("generated PolicyType discriminant should decode");
        }
    }

    #[test]
    fn policy_call_labels_are_stable() {
        assert_eq!(
            IPolicyRegistry::IPolicyRegistryCalls::isAuthorized(
                IPolicyRegistry::isAuthorizedCall { policyId: 0, account: Address::ZERO },
            )
            .as_label(),
            "policy.isAuthorized"
        );
    }
}
