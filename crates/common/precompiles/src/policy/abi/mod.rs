//! Wire (ABI) surfaces for the `PolicyRegistry` precompile, one per hardfork that moved them.
//!
//! The latest surface is always named `IPolicyRegistry` in its `vN` module, then re-exported here
//! as both [`IPolicyRegistry`] (canonical) and `IPolicyRegistryVN`. Older forks keep the same
//! Rust name inside their module so truncated-calldata revert bytes stay stable, and are re-exported
//! as [`IPolicyRegistryV1`], [`IPolicyRegistryV2`], etc.

mod v1;
pub use v1::IPolicyRegistry as IPolicyRegistryV1;

mod v2;
pub use v2::{IPolicyRegistry, IPolicyRegistry as IPolicyRegistryV2};

/// Lifts a Beryl-frozen policy call into the canonical (Cobalt) enum without re-parsing calldata.
///
/// V1 selectors are a subset of V2. Shared layouts move; `PolicyType` is remapped by name (V1
/// never carried `UNION` / `INTERSECT`).
impl From<IPolicyRegistryV1::IPolicyRegistryCalls> for IPolicyRegistry::IPolicyRegistryCalls {
    fn from(call: IPolicyRegistryV1::IPolicyRegistryCalls) -> Self {
        match call {
            IPolicyRegistryV1::IPolicyRegistryCalls::createPolicy(c) => {
                Self::createPolicy(IPolicyRegistry::createPolicyCall {
                    admin: c.admin,
                    policyType: lift_policy_type(c.policyType),
                })
            }
            IPolicyRegistryV1::IPolicyRegistryCalls::createPolicyWithAccounts(c) => {
                Self::createPolicyWithAccounts(IPolicyRegistry::createPolicyWithAccountsCall {
                    admin: c.admin,
                    policyType: lift_policy_type(c.policyType),
                    accounts: c.accounts,
                })
            }
            IPolicyRegistryV1::IPolicyRegistryCalls::stageUpdateAdmin(c) => {
                Self::stageUpdateAdmin(IPolicyRegistry::stageUpdateAdminCall {
                    policyId: c.policyId,
                    newAdmin: c.newAdmin,
                })
            }
            IPolicyRegistryV1::IPolicyRegistryCalls::finalizeUpdateAdmin(c) => {
                Self::finalizeUpdateAdmin(IPolicyRegistry::finalizeUpdateAdminCall {
                    policyId: c.policyId,
                })
            }
            IPolicyRegistryV1::IPolicyRegistryCalls::renounceAdmin(c) => {
                Self::renounceAdmin(IPolicyRegistry::renounceAdminCall { policyId: c.policyId })
            }
            IPolicyRegistryV1::IPolicyRegistryCalls::updateAllowlist(c) => {
                Self::updateAllowlist(IPolicyRegistry::updateAllowlistCall {
                    policyId: c.policyId,
                    allowed: c.allowed,
                    accounts: c.accounts,
                })
            }
            IPolicyRegistryV1::IPolicyRegistryCalls::updateBlocklist(c) => {
                Self::updateBlocklist(IPolicyRegistry::updateBlocklistCall {
                    policyId: c.policyId,
                    blocked: c.blocked,
                    accounts: c.accounts,
                })
            }
            IPolicyRegistryV1::IPolicyRegistryCalls::isAuthorized(c) => {
                Self::isAuthorized(IPolicyRegistry::isAuthorizedCall {
                    policyId: c.policyId,
                    account: c.account,
                })
            }
            IPolicyRegistryV1::IPolicyRegistryCalls::policyExists(c) => {
                Self::policyExists(IPolicyRegistry::policyExistsCall { policyId: c.policyId })
            }
            IPolicyRegistryV1::IPolicyRegistryCalls::policyAdmin(c) => {
                Self::policyAdmin(IPolicyRegistry::policyAdminCall { policyId: c.policyId })
            }
            IPolicyRegistryV1::IPolicyRegistryCalls::pendingPolicyAdmin(c) => {
                Self::pendingPolicyAdmin(IPolicyRegistry::pendingPolicyAdminCall {
                    policyId: c.policyId,
                })
            }
        }
    }
}

fn lift_policy_type(policy_type: IPolicyRegistryV1::PolicyType) -> IPolicyRegistry::PolicyType {
    IPolicyRegistry::PolicyType::try_from(policy_type as u8)
        .expect("V1 PolicyType discriminant must lift into V2")
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
            Self::MIN_COMPOSITE_CHILD_POLICIES(_) => "policy.MIN_COMPOSITE_CHILD_POLICIES",
            Self::MAX_COMPOSITE_CHILD_POLICIES(_) => "policy.MAX_COMPOSITE_CHILD_POLICIES",
            Self::policyExists(_) => "policy.policyExists",
            Self::policyAdmin(_) => "policy.policyAdmin",
            Self::pendingPolicyAdmin(_) => "policy.pendingPolicyAdmin",
            Self::compositePolicyChildIds(_) => "policy.compositePolicyChildIds",
        }
    }
}

impl IPolicyRegistry::PolicyType {
    /// Returns the raw `u8` discriminant for this policy type.
    pub const fn as_discriminant(self) -> u8 {
        self as u8
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, b256};
    use alloy_sol_types::{SolEnum, SolError, SolEvent, SolInterface};

    use super::{IPolicyRegistry, IPolicyRegistryV1};
    use crate::AbiFingerprint;

    /// Absolute wire fingerprint for Beryl's surface. Catches both-sides drift that relative
    /// V1==V2 asserts miss (alloy Display / signature changes that move every copy together).
    const V1_ABI_FINGERPRINT: B256 =
        b256!("1ae189209c8c4875de2caa707322ea74f0d1f3e74a1104ecee6884e8984415da");

    /// Absolute wire fingerprint for Cobalt's (canonical) surface.
    const V2_ABI_FINGERPRINT: B256 =
        b256!("da3137a81688286fb3af7f0f09a6369ae7c1197c08844a29dbde13f8c036394d");

    /// These two surfaces pass no enum ordinals to [`AbiFingerprint`], so the pinned constants
    /// above keep the values they were blessed with. `PolicyType` ordinals *are* load-bearing —
    /// the discriminant rides the top byte of every policy ID via `PolicyRegistryV1::make_id` —
    /// and `shared_policy_type_discriminants_agree_across_surfaces` below only catches a reorder
    /// of one surface, not a simultaneous reorder of both. Feeding the ordinals in here would
    /// close that, at the cost of re-blessing both constants; deliberately left as a follow-up.
    fn v1_abi_fingerprint() -> B256 {
        AbiFingerprint::compute(
            IPolicyRegistryV1::IPolicyRegistryCalls::selectors(),
            IPolicyRegistryV1::IPolicyRegistryEvents::SELECTORS.iter().copied().map(B256::new),
            IPolicyRegistryV1::IPolicyRegistryErrors::selectors(),
            IPolicyRegistryV1::PolicyType::COUNT,
            [],
        )
    }

    fn v2_abi_fingerprint() -> B256 {
        AbiFingerprint::compute(
            IPolicyRegistry::IPolicyRegistryCalls::selectors(),
            IPolicyRegistry::IPolicyRegistryEvents::SELECTORS.iter().copied().map(B256::new),
            IPolicyRegistry::IPolicyRegistryErrors::selectors(),
            IPolicyRegistry::PolicyType::COUNT,
            [],
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

    #[test]
    fn every_policy_type_discriminant_decodes() {
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
