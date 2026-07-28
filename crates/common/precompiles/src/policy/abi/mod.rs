//! Wire (ABI) surfaces for the `PolicyRegistry` precompile, one per hardfork that moved them.
//!
//! [`IPolicyRegistry`] is the canonical live surface. [`IPolicyRegistryV1`] and
//! [`IPolicyRegistryV2`] are the frozen per-fork surfaces, selected on the execution path by
//! [`crate::PolicyAbi`] so that each block decodes against the surface that was dialable when it
//! was produced. That is what a selector-keyed fork gate cannot do: appending `UNION`/`INTERSECT`
//! to `PolicyType` left `createPolicy(address,uint8)` hashing to the same selector while changing
//! which discriminants the decoder accepts.
//!
//! # Canonical aliases the newest frozen surface
//!
//! Canonical is not a third copy — it is a re-export of the newest `vN`, so the two cannot drift
//! and no equality test is needed to keep them honest. Every generated nested name
//! (`IPolicyRegistryCalls`, `PolicyType`, `createPolicyCall`, …) therefore keeps its exact spelling
//! for the whole crate and its consumers.
//!
//! A fork that changes the wire adds `abi/vN.rs` and retargets the canonical alias below. A fork
//! that only changes logic adds nothing here; [`crate::PolicyVersion::abi`] maps it onto the
//! existing surface. The two axes grow independently and meet only in `versions.rs`.
//!
//! # Both surfaces must stay named `IPolicyRegistry`
//!
//! `SolInterface::abi_decode_validate` short-circuits on `data.len() < MIN_DATA_LENGTH + 4` with
//! `Error::type_check_fail(data, Self::NAME)`, and `BasePrecompileError::AbiDecodeFailed` encodes
//! as `selector || utf8(error)`. `NAME` is `"{interface}Calls"`, so the interface's Rust name is
//! consensus data on every truncated-calldata revert. Naming a frozen surface `IPolicyRegistryV1`
//! would silently change those bytes.

mod v1;
pub use v1::IPolicyRegistry as IPolicyRegistryV1;

mod v2;
pub use v2::{IPolicyRegistry, IPolicyRegistry as IPolicyRegistryV2};

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
    use alloy_sol_types::{SolEnum, SolError, SolEvent};

    use super::{IPolicyRegistry, IPolicyRegistryV1};

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

    /// The leaf discriminants must mean the same thing on both surfaces. `PolicyType` rides the
    /// top byte of every policy ID (`PolicyRegistryV1::make_id`), so a reordering would silently
    /// reinterpret every stored policy.
    #[test]
    fn shared_policy_type_discriminants_agree_across_surfaces() {
        assert_eq!(
            IPolicyRegistryV1::PolicyType::BLOCKLIST as u8,
            IPolicyRegistry::PolicyType::BLOCKLIST as u8
        );
        assert_eq!(
            IPolicyRegistryV1::PolicyType::ALLOWLIST as u8,
            IPolicyRegistry::PolicyType::ALLOWLIST as u8
        );
    }

    /// Events and errors carried over from Beryl keep their topic0 / selector. A signature drift
    /// here would change the logs and revert data of ops that both surfaces share.
    #[test]
    fn shared_events_and_errors_keep_their_signatures() {
        assert_eq!(
            IPolicyRegistryV1::PolicyCreated::SIGNATURE_HASH,
            IPolicyRegistry::PolicyCreated::SIGNATURE_HASH
        );
        assert_eq!(
            IPolicyRegistryV1::PolicyAdminStaged::SIGNATURE_HASH,
            IPolicyRegistry::PolicyAdminStaged::SIGNATURE_HASH
        );
        assert_eq!(
            IPolicyRegistryV1::PolicyAdminUpdated::SIGNATURE_HASH,
            IPolicyRegistry::PolicyAdminUpdated::SIGNATURE_HASH
        );
        assert_eq!(
            IPolicyRegistryV1::AllowlistUpdated::SIGNATURE_HASH,
            IPolicyRegistry::AllowlistUpdated::SIGNATURE_HASH
        );
        assert_eq!(
            IPolicyRegistryV1::BlocklistUpdated::SIGNATURE_HASH,
            IPolicyRegistry::BlocklistUpdated::SIGNATURE_HASH
        );

        assert_eq!(IPolicyRegistryV1::NonPayable::SELECTOR, IPolicyRegistry::NonPayable::SELECTOR);
        assert_eq!(
            IPolicyRegistryV1::Unauthorized::SELECTOR,
            IPolicyRegistry::Unauthorized::SELECTOR
        );
        assert_eq!(
            IPolicyRegistryV1::PolicyNotFound::SELECTOR,
            IPolicyRegistry::PolicyNotFound::SELECTOR
        );
        assert_eq!(
            IPolicyRegistryV1::IncompatiblePolicyType::SELECTOR,
            IPolicyRegistry::IncompatiblePolicyType::SELECTOR
        );
        assert_eq!(
            IPolicyRegistryV1::ZeroAddress::SELECTOR,
            IPolicyRegistry::ZeroAddress::SELECTOR
        );
        assert_eq!(
            IPolicyRegistryV1::BatchSizeTooLarge::SELECTOR,
            IPolicyRegistry::BatchSizeTooLarge::SELECTOR
        );
        assert_eq!(
            IPolicyRegistryV1::NoPendingAdmin::SELECTOR,
            IPolicyRegistry::NoPendingAdmin::SELECTOR
        );
    }
}
