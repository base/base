//! Golden tests pinning Policy Registry **V2** behavior of the B-20 precompile.
//!
//! V2 is activated at Cobalt as a behavior-preserving copy of V1 (a scaffold seam for future
//! Cobalt-era changes). Its logic and storage layout are identical to V1, so this suite pins its
//! own roots to lock V2's behavior independently: a future edit that alters V2 must re-bless these
//! roots. These goldens run production Cobalt storage features (see below) while the V1 suite runs
//! Legacy, so a root matches V1's only when the op does not trigger Cobalt's dynamic tail cleanup;
//! ops that write shrinking dynamic values (e.g. allow/block membership vecs) can legitimately
//! diverge from V1's Legacy pin.
//!
//! Every op (policy creation, admin lifecycle, allow/block membership, and evaluation reads) is
//! driven through the real `PolicyRegistryStorage` entry (version-resolver-gated `dispatch`,
//! `BaseUpgrade::Cobalt` -> `PolicyVersion::V2`) over `HashMapStorageProvider` configured with
//! `StorageFeatures::Cobalt` (the production storage config at Cobalt).
//! Each case asserts:
//!   1. exact returned ABI bytes (or the typed revert),
//!   2. resulting registry state,
//!   3. emitted events, and
//!   4. a per-case keccak storage **hash** snapshot (the frozen-manifest baseline), scoped to the
//!      registry address so it is independent of activation-registry scaffolding.
//!
//! Write ops require the `PolicyRegistry` activation feature; `fresh()` activates it. Read ops
//! bypass activation. Custom policy IDs encode `(type << 56) | counter`, with the shared counter
//! starting at 2 (built-ins `ALWAYS_ALLOW` = 0 and `ALWAYS_BLOCK` consume 0 and 1).
//!
//! ## Blessing storage hashes
//! State-hash constants below are pinned. To (re)generate them after an intentional change, run:
//! `BLESS_GOLDEN=1 cargo test -p base-common-precompiles --features test-utils \
//!    --test b20_policy_v2_golden -- --nocapture` and copy the printed `GOLDEN_ROOT` values.

use IPolicyRegistry::PolicyType;
use alloy_primitives::{Address, B256, Bytes, LogData, U256, b256, keccak256};
use alloy_sol_types::{SolCall, SolError, SolEvent};
use base_common_genesis::BaseUpgrade;
use base_common_precompiles::{
    ActivationAdminConfig, ActivationFeature, ActivationRegistryStorage, IPolicyRegistry,
    PolicyRegistryStorage, PolicyVersion, PolicyVersions,
};
use base_precompile_storage::{HashMapStorageProvider, StorageCtx, StorageFeatures};

mod common;
use common::{
    ACTIVATION_ADMIN, ADMIN, ALICE, BOB, CHAIN_ID, bless_or_assert_gas, bless_or_assert_root,
};

// --- fixtures ---------------------------------------------------------------

const ADMIN2: Address = Address::repeat_byte(0xA2);
const OUTSIDER: Address = Address::repeat_byte(0x0F);

/// First custom BLOCKLIST id in a fresh registry: `(0 << 56) | 2`.
const BLOCKLIST_ID: u64 = 2;
/// First custom ALLOWLIST id in a fresh registry: `(1 << 56) | 2`.
const ALLOWLIST_ID: u64 = (1u64 << 56) | 2;

// --- pinned storage hashes (bless with BLESS_GOLDEN=1; see module docs) --------
//
// V2 is a behavior- and layout-preserving copy of V1, so these track V1's roots — but pinned
// independently here: these goldens run `StorageFeatures::Cobalt` while the V1 suite runs Legacy,
// so an op that triggers Cobalt dynamic tail cleanup can diverge from its V1 pin. Re-blessing
// reflects the true Cobalt snapshot for each.

const ROOT_CREATE_BLOCKLIST: B256 =
    b256!("5ff0dab60b6daec34cbc6135f09097ddbbe31c6f662d4cdd9c6c4c7b5a589556");
const ROOT_CREATE_ALLOWLIST: B256 =
    b256!("d3897ed8f0dd86ed6a036e2c1386e43d3a54adfc3cf79a05bdd69fbabcdf402f");
const ROOT_CREATE_WITH_ACCOUNTS: B256 =
    b256!("5d5964977121364f0c16ef145e1edcafe296cfed6e5a932506cb10a35fa969c1");
const ROOT_CREATE_WITH_ACCOUNTS_BLOCKLIST: B256 =
    b256!("a7935cb41217a504a2ca9bd9517aebaaef9d54fe58f685bd569a590749f3d569");
const ROOT_UPDATE_ALLOWLIST: B256 =
    b256!("ae7e56989c3a1dab0b1de0b272dad0381a62414917d9fd82541f35764087748e");
const ROOT_UPDATE_ALLOWLIST_REMOVE: B256 =
    b256!("26874cf1f9e3b7a06e33cc25b500e0fe8fc03c084bb2d6eee390e6adb5cb879d");
const ROOT_UPDATE_BLOCKLIST: B256 =
    b256!("a7935cb41217a504a2ca9bd9517aebaaef9d54fe58f685bd569a590749f3d569");
const ROOT_STAGE_FINALIZE_ADMIN: B256 =
    b256!("8af193788c98e688ce3bb069b241f1fea51bd8dc6d9aa95995bcd956071a111d");
const ROOT_RENOUNCE_ADMIN: B256 =
    b256!("5d31ed7e17637df947521d71d43d5d9b93e75b070e6ffeee6e442ac9a2ce0e5b");
// Composite roots are V2-only (no V1 equivalent). Bless with BLESS_GOLDEN=1.
const ROOT_CREATE_COMPOSITE_UNION: B256 =
    b256!("f76b88e4cd8e2379cd8fe082a8530e187eca346599b679abb5038aaf62d048ee");
const ROOT_UPDATE_COMPOSITE: B256 =
    b256!("fa1912098beb387418d4c28bf4d9cc499f0f2cf8a623e6d57a768bcf062c9094");

// --- harness ----------------------------------------------------------------

/// The policy registry precompile's singleton address.
const fn registry() -> Address {
    PolicyRegistryStorage::ADDRESS
}

/// Activates the `PolicyRegistry` feature so write ops are permitted.
fn activate(storage: &mut HashMapStorageProvider) {
    storage.set_caller(ACTIVATION_ADMIN);
    StorageCtx::enter(storage, |ctx| {
        ActivationRegistryStorage::new(ctx)
            .activate(
                ActivationFeature::PolicyRegistry.id(),
                ActivationAdminConfig::static_fallback(Some(ACTIVATION_ADMIN)),
            )
            .unwrap();
    });
}

/// A fresh provider with the policy registry activated.
fn fresh() -> HashMapStorageProvider {
    let mut storage = HashMapStorageProvider::new_with_storage_features(CHAIN_ID, StorageFeatures::Cobalt);
    activate(&mut storage);
    storage
}

/// Drives one registry call through `dispatch`, returning `(is_revert, bytes)`.
fn call_policy(
    storage: &mut HashMapStorageProvider,
    caller: Address,
    calldata: Vec<u8>,
) -> (bool, Bytes) {
    storage.set_caller(caller);
    StorageCtx::enter(storage, |ctx| {
        PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata, BaseUpgrade::Cobalt)
    })
    .map(|out| (out.is_revert(), out.bytes))
    .expect("dispatch must not fatally error")
}

/// Creates a policy and returns its decoded id.
fn create(
    storage: &mut HashMapStorageProvider,
    caller: Address,
    admin: Address,
    policy_type: PolicyType,
) -> u64 {
    let (rev, bytes) = call_policy(
        storage,
        caller,
        IPolicyRegistry::createPolicyCall { admin, policyType: policy_type }.abi_encode(),
    );
    assert!(!rev, "createPolicy reverted");
    IPolicyRegistry::createPolicyCall::abi_decode_returns(&bytes).unwrap()
}

/// Reads `isAuthorized(policy_id, account)`.
fn is_authorized(storage: &mut HashMapStorageProvider, policy_id: u64, account: Address) -> bool {
    let (rev, bytes) = call_policy(
        storage,
        OUTSIDER,
        IPolicyRegistry::isAuthorizedCall { policyId: policy_id, account }.abi_encode(),
    );
    assert!(!rev);
    IPolicyRegistry::isAuthorizedCall::abi_decode_returns(&bytes).unwrap()
}

/// Deterministic keccak hash of the per-case snapshot, scoped to the registry address: its emitted
/// events (topics + data) followed by its sorted `(slot, value)` storage entries. Scoping excludes
/// activation-registry scaffolding, so the pin captures only the registry's own effect.
fn hash_state(storage: HashMapStorageProvider) -> B256 {
    let mut buf = Vec::new();
    let events: Vec<LogData> = storage.get_events(registry()).clone();
    for log in &events {
        for topic in log.topics() {
            buf.extend_from_slice(topic.as_slice());
        }
        buf.extend_from_slice(&log.data);
    }
    let mut triples: Vec<(Address, U256, U256)> =
        storage.into_storage().filter(|(a, _, _)| *a == registry()).collect();
    triples.sort();
    for (_addr, slot, value) in triples {
        buf.extend_from_slice(&slot.to_be_bytes::<32>());
        buf.extend_from_slice(&value.to_be_bytes::<32>());
    }
    keccak256(&buf)
}

/// Asserts the storage hash, or prints it under `BLESS_GOLDEN` for (re)pinning.
#[track_caller]
fn assert_root(label: &str, storage: HashMapStorageProvider, expected: B256) {
    bless_or_assert_root(label, hash_state(storage), expected);
}

// ============================================================================
// Version resolver
// ============================================================================

#[test]
fn resolver_maps_forks_to_versions() {
    assert_eq!(PolicyVersions::from_base_upgrade(BaseUpgrade::Azul), None);
    assert_eq!(PolicyVersions::from_base_upgrade(BaseUpgrade::Beryl), Some(PolicyVersion::V1));
    assert_eq!(PolicyVersions::from_base_upgrade(BaseUpgrade::Cobalt), Some(PolicyVersion::V2));
}

#[test]
fn dispatch_reverts_before_beryl() {
    let mut s = fresh();
    let calldata = IPolicyRegistry::isAuthorizedCall { policyId: 0, account: ALICE }.abi_encode();
    s.set_caller(ALICE);
    let (rev, bytes) = StorageCtx::enter(&mut s, |ctx| {
        PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata, BaseUpgrade::Azul)
    })
    .map(|out| (out.is_revert(), out.bytes))
    .expect("dispatch must not fatally error");
    assert!(rev);
    assert!(bytes.is_empty());
}

// ============================================================================
// evaluation reads (isAuthorized / built-ins / empty policies)
// ============================================================================

#[test]
fn golden_is_authorized_builtins_and_malformed() {
    let mut s = fresh();
    // ALWAYS_ALLOW (0) authorizes everyone; ALWAYS_BLOCK rejects everyone.
    assert!(is_authorized(&mut s, PolicyRegistryStorage::ALWAYS_ALLOW_ID, ALICE));
    assert!(!is_authorized(&mut s, PolicyRegistryStorage::ALWAYS_BLOCK_ID, ALICE));
    // Malformed id (type byte > INTERSECT) is unauthorized, never reverts. Type byte 2 is now
    // UNION, so use 4 to stay above the composite range.
    assert!(!is_authorized(&mut s, 4u64 << 56, ALICE));
}

#[test]
fn golden_is_authorized_empty_policies() {
    let mut s = fresh();
    // Empty BLOCKLIST authorizes everyone; empty ALLOWLIST rejects everyone.
    let block = create(&mut s, ADMIN, ADMIN, PolicyType::BLOCKLIST);
    let allow = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    assert!(is_authorized(&mut s, block, ALICE));
    assert!(!is_authorized(&mut s, allow, ALICE));
}

#[test]
fn golden_reads_for_nonexistent_and_builtins() {
    let mut s = fresh();
    // policyExists: false for unknown custom id, true for built-ins.
    let (_, bytes) = call_policy(
        &mut s,
        OUTSIDER,
        IPolicyRegistry::policyExistsCall { policyId: 999 }.abi_encode(),
    );
    assert_eq!(bytes, Bytes::from(IPolicyRegistry::policyExistsCall::abi_encode_returns(&false)));
    let (_, bytes) = call_policy(
        &mut s,
        OUTSIDER,
        IPolicyRegistry::policyExistsCall { policyId: PolicyRegistryStorage::ALWAYS_ALLOW_ID }
            .abi_encode(),
    );
    assert_eq!(bytes, Bytes::from(IPolicyRegistry::policyExistsCall::abi_encode_returns(&true)));
    // policyAdmin / pendingPolicyAdmin: zero for nonexistent, never revert.
    let (_, bytes) = call_policy(
        &mut s,
        OUTSIDER,
        IPolicyRegistry::policyAdminCall { policyId: 999 }.abi_encode(),
    );
    assert_eq!(
        bytes,
        Bytes::from(IPolicyRegistry::policyAdminCall::abi_encode_returns(&Address::ZERO))
    );
    let (_, bytes) = call_policy(
        &mut s,
        OUTSIDER,
        IPolicyRegistry::pendingPolicyAdminCall { policyId: 999 }.abi_encode(),
    );
    assert_eq!(
        bytes,
        Bytes::from(IPolicyRegistry::pendingPolicyAdminCall::abi_encode_returns(&Address::ZERO))
    );
}

// ============================================================================
// createPolicy
// ============================================================================

#[test]
fn golden_create_blocklist() {
    let mut s = fresh();
    let (rev, bytes) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::createPolicyCall { admin: ADMIN, policyType: PolicyType::BLOCKLIST }
            .abi_encode(),
    );
    assert!(!rev);
    assert_eq!(
        bytes,
        Bytes::from(IPolicyRegistry::createPolicyCall::abi_encode_returns(&BLOCKLIST_ID))
    );
    let (_, exists) = call_policy(
        &mut s,
        OUTSIDER,
        IPolicyRegistry::policyExistsCall { policyId: BLOCKLIST_ID }.abi_encode(),
    );
    assert_eq!(exists, Bytes::from(IPolicyRegistry::policyExistsCall::abi_encode_returns(&true)));
    // createPolicy emits PolicyCreated then PolicyAdminUpdated (0 -> admin).
    let events = s.get_events(registry());
    assert_eq!(
        events[events.len() - 2].topics()[0],
        IPolicyRegistry::PolicyCreated::SIGNATURE_HASH
    );
    assert_eq!(
        events[events.len() - 1].topics()[0],
        IPolicyRegistry::PolicyAdminUpdated::SIGNATURE_HASH
    );
    assert_root("create_blocklist", s, ROOT_CREATE_BLOCKLIST);
}

#[test]
fn golden_create_allowlist() {
    let mut s = fresh();
    let (rev, bytes) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::createPolicyCall { admin: ADMIN, policyType: PolicyType::ALLOWLIST }
            .abi_encode(),
    );
    assert!(!rev);
    assert_eq!(
        bytes,
        Bytes::from(IPolicyRegistry::createPolicyCall::abi_encode_returns(&ALLOWLIST_ID))
    );
    let (_, admin) = call_policy(
        &mut s,
        OUTSIDER,
        IPolicyRegistry::policyAdminCall { policyId: ALLOWLIST_ID }.abi_encode(),
    );
    assert_eq!(admin, Bytes::from(IPolicyRegistry::policyAdminCall::abi_encode_returns(&ADMIN)));
    let events = s.get_events(registry());
    assert_eq!(
        events[events.len() - 2].topics()[0],
        IPolicyRegistry::PolicyCreated::SIGNATURE_HASH
    );
    assert_eq!(
        events[events.len() - 1].topics()[0],
        IPolicyRegistry::PolicyAdminUpdated::SIGNATURE_HASH
    );
    assert_root("create_allowlist", s, ROOT_CREATE_ALLOWLIST);
}

#[test]
fn golden_create_reverts_zero_admin() {
    let mut s = fresh();
    let (rev, bytes) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::createPolicyCall {
            admin: Address::ZERO,
            policyType: PolicyType::BLOCKLIST,
        }
        .abi_encode(),
    );
    assert!(rev);
    assert_eq!(bytes, Bytes::from(IPolicyRegistry::ZeroAddress {}.abi_encode()));
}

/// A composite discriminant (UNION/INTERSECT) decodes at V2 (the enum widened at Cobalt) and
/// reaches the logic, which rejects it with `IncompatiblePolicyType` rather than a `Panic(0x21)`.
/// The revert body is exactly the 4-byte selector (no args); this is a revert-only path, so no
/// state-root golden moves.
#[test]
fn golden_create_policy_composite_type_is_incompatible_policy_type_in_v2() {
    for policy_type in [PolicyType::UNION, PolicyType::INTERSECT] {
        let calldata = IPolicyRegistry::createPolicyCall { admin: ADMIN, policyType: policy_type }
            .abi_encode();
        let mut s = fresh();
        let (rev, bytes) = call_policy(&mut s, ADMIN, calldata);
        assert!(rev);
        assert_eq!(bytes, Bytes::from(IPolicyRegistry::IncompatiblePolicyType::SELECTOR.as_ref()));
    }
}

// ============================================================================
// composite policies (V2: UNION / INTERSECT)
// ============================================================================

/// Adds `account` to allowlist policy `id` as its admin (`ADMIN`).
fn set_allow(s: &mut HashMapStorageProvider, id: u64, account: Address) {
    let (rev, _) = call_policy(
        s,
        ADMIN,
        IPolicyRegistry::updateAllowlistCall {
            policyId: id,
            allowed: true,
            accounts: vec![account],
        }
        .abi_encode(),
    );
    assert!(!rev, "updateAllowlist setup unexpectedly reverted");
}

/// Creates a composite policy as `ADMIN`, returning its decoded id.
fn create_composite(s: &mut HashMapStorageProvider, gate: PolicyType, children: Vec<u64>) -> u64 {
    let (rev, bytes) = call_policy(
        s,
        ADMIN,
        IPolicyRegistry::createCompositePolicyCall {
            admin: ADMIN,
            policyType: gate,
            childPolicyIds: children,
        }
        .abi_encode(),
    );
    assert!(!rev, "createCompositePolicy unexpectedly reverted");
    IPolicyRegistry::createCompositePolicyCall::abi_decode_returns(&bytes).unwrap()
}

#[test]
fn golden_create_composite_union() {
    let mut s = fresh();
    let a = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    let b = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    set_allow(&mut s, a, ALICE);
    set_allow(&mut s, b, BOB);

    let id = create_composite(&mut s, PolicyType::UNION, vec![a, b]);
    assert_eq!((id >> 56) as u8, PolicyType::UNION as u8);

    // Live UNION: authorized if ANY child authorizes.
    assert!(is_authorized(&mut s, id, ALICE));
    assert!(is_authorized(&mut s, id, BOB));
    assert!(!is_authorized(&mut s, id, OUTSIDER));

    // Creation emits PolicyCreated, PolicyAdminUpdated, then CompositePolicyUpdated last.
    assert_eq!(
        s.get_events(registry()).last().unwrap().topics()[0],
        IPolicyRegistry::CompositePolicyUpdated::SIGNATURE_HASH
    );
    assert_root("create_composite_union", s, ROOT_CREATE_COMPOSITE_UNION);
}

#[test]
fn golden_update_composite() {
    let mut s = fresh();
    let a = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    let b = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    let c = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    set_allow(&mut s, a, ALICE);
    set_allow(&mut s, c, BOB);

    let id = create_composite(&mut s, PolicyType::UNION, vec![a, b]);
    assert!(is_authorized(&mut s, id, ALICE));
    assert!(!is_authorized(&mut s, id, BOB));

    // Replace [a, b] with [b, c]: ALICE (only in a) drops out, BOB (in c) joins.
    let (rev, bytes) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::updateCompositeCall { policyId: id, childPolicyIds: vec![b, c] }
            .abi_encode(),
    );
    assert!(!rev);
    assert!(bytes.is_empty());
    assert!(!is_authorized(&mut s, id, ALICE));
    assert!(is_authorized(&mut s, id, BOB));
    assert_eq!(
        s.get_events(registry()).last().unwrap().topics()[0],
        IPolicyRegistry::CompositePolicyUpdated::SIGNATURE_HASH
    );
    assert_root("update_composite", s, ROOT_UPDATE_COMPOSITE);
}

/// Reads a composite's child set over the wire, asserting the call never reverts.
fn composite_child_ids(storage: &mut HashMapStorageProvider, policy_id: u64) -> Vec<u64> {
    let (rev, bytes) = call_policy(
        storage,
        OUTSIDER,
        IPolicyRegistry::compositePolicyChildIdsCall { policyId: policy_id }.abi_encode(),
    );
    assert!(!rev, "compositePolicyChildIds unexpectedly reverted");
    IPolicyRegistry::compositePolicyChildIdsCall::abi_decode_returns(&bytes).unwrap()
}

#[test]
fn golden_composite_policy_child_ids() {
    let mut s = fresh();
    let a = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    let b = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    let c = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    let id = create_composite(&mut s, PolicyType::INTERSECT, vec![c, a]);

    // Order is preserved verbatim — the registry neither sorts nor de-dupes.
    assert_eq!(composite_child_ids(&mut s, id), vec![c, a]);

    // The view tracks a full replacement, leaving no stale tail behind.
    let (rev, _) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::updateCompositeCall { policyId: id, childPolicyIds: vec![a, b, c] }
            .abi_encode(),
    );
    assert!(!rev);
    assert_eq!(composite_child_ids(&mut s, id), vec![a, b, c]);

    // The view agrees with the payload of the event it mirrors.
    let emitted = IPolicyRegistry::CompositePolicyUpdated::decode_log_data(
        s.get_events(registry()).last().unwrap(),
    )
    .unwrap()
    .childPolicyIds;
    assert_eq!(composite_child_ids(&mut s, id), emitted);
}

/// The view is total: every non-composite input yields an empty array rather than a revert,
/// matching the never-reverts contract the rest of the read surface documents.
#[test]
fn golden_composite_policy_child_ids_empty_for_non_composites() {
    let mut s = fresh();
    let simple = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    // Type byte above INTERSECT — malformed, and never a valid stored policy.
    let malformed = (9u64 << 56) | 7;
    // Well-formed UNION id that was never created.
    let uncreated = ((PolicyType::UNION as u64) << 56) | 999;

    for policy_id in [
        simple,
        PolicyRegistryStorage::ALWAYS_ALLOW_ID,
        PolicyRegistryStorage::ALWAYS_BLOCK_ID,
        malformed,
        uncreated,
    ] {
        assert_eq!(
            composite_child_ids(&mut s, policy_id),
            Vec::<u64>::new(),
            "expected an empty child set for policy {policy_id}"
        );
    }
}

/// The view bypasses the activation gate, like every other read on this surface.
#[test]
fn golden_composite_policy_child_ids_reads_before_activation() {
    // No activation, matching `golden_write_reverts_when_not_activated`.
    let mut s = HashMapStorageProvider::new_with_storage_features(CHAIN_ID, StorageFeatures::Cobalt);
    let (rev, bytes) = call_policy(
        &mut s,
        OUTSIDER,
        IPolicyRegistry::compositePolicyChildIdsCall { policyId: 999 }.abi_encode(),
    );
    assert!(!rev, "view must not be gated on activation");
    assert_eq!(
        bytes,
        Bytes::from(IPolicyRegistry::compositePolicyChildIdsCall::abi_encode_returns(&Vec::new()))
    );
}

#[test]
fn golden_create_composite_reverts_incompatible_type() {
    let mut s = fresh();
    let a = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    let b = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    let (rev, bytes) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::createCompositePolicyCall {
            admin: ADMIN,
            policyType: PolicyType::ALLOWLIST,
            childPolicyIds: vec![a, b],
        }
        .abi_encode(),
    );
    assert!(rev);
    assert_eq!(bytes, Bytes::from(IPolicyRegistry::IncompatiblePolicyType {}.abi_encode()));
}

#[test]
fn golden_update_composite_reverts_unauthorized() {
    let mut s = fresh();
    let a = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    let b = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    let id = create_composite(&mut s, PolicyType::UNION, vec![a, b]);
    // OUTSIDER is not the composite's admin.
    let (rev, bytes) = call_policy(
        &mut s,
        OUTSIDER,
        IPolicyRegistry::updateCompositeCall { policyId: id, childPolicyIds: vec![a, b] }
            .abi_encode(),
    );
    assert!(rev);
    assert_eq!(bytes, Bytes::from(IPolicyRegistry::Unauthorized {}.abi_encode()));
}

#[test]
fn golden_create_with_accounts() {
    let mut s = fresh();
    let (rev, bytes) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::createPolicyWithAccountsCall {
            admin: ADMIN,
            policyType: PolicyType::ALLOWLIST,
            accounts: vec![ALICE, BOB],
        }
        .abi_encode(),
    );
    assert!(!rev);
    let id = IPolicyRegistry::createPolicyWithAccountsCall::abi_decode_returns(&bytes).unwrap();
    assert_eq!(id, ALLOWLIST_ID);
    assert!(is_authorized(&mut s, id, ALICE));
    assert!(is_authorized(&mut s, id, BOB));
    assert!(!is_authorized(&mut s, id, OUTSIDER));
    assert_eq!(
        s.get_events(registry()).last().unwrap().topics()[0],
        IPolicyRegistry::AllowlistUpdated::SIGNATURE_HASH
    );
    assert_root("create_with_accounts", s, ROOT_CREATE_WITH_ACCOUNTS);
}

#[test]
fn golden_create_with_accounts_reverts_batch_too_large() {
    let mut s = fresh();
    let (rev, bytes) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::createPolicyWithAccountsCall {
            admin: ADMIN,
            policyType: PolicyType::ALLOWLIST,
            accounts: vec![ALICE; 65],
        }
        .abi_encode(),
    );
    assert!(rev);
    assert_eq!(
        bytes,
        Bytes::from(
            IPolicyRegistry::BatchSizeTooLarge { maxBatchSize: U256::from(64) }.abi_encode()
        )
    );
}

#[test]
fn golden_create_with_accounts_blocklist() {
    let mut s = fresh();
    let (rev, bytes) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::createPolicyWithAccountsCall {
            admin: ADMIN,
            policyType: PolicyType::BLOCKLIST,
            accounts: vec![ALICE],
        }
        .abi_encode(),
    );
    assert!(!rev);
    let id = IPolicyRegistry::createPolicyWithAccountsCall::abi_decode_returns(&bytes).unwrap();
    assert_eq!(id, BLOCKLIST_ID);
    // Blocked account rejected; everyone else authorized (empty-blocklist default).
    assert!(!is_authorized(&mut s, id, ALICE));
    assert!(is_authorized(&mut s, id, BOB));
    assert_eq!(
        s.get_events(registry()).last().unwrap().topics()[0],
        IPolicyRegistry::BlocklistUpdated::SIGNATURE_HASH
    );
    assert_root("create_with_accounts_blocklist", s, ROOT_CREATE_WITH_ACCOUNTS_BLOCKLIST);
}

// ============================================================================
// updateAllowlist / updateBlocklist
// ============================================================================

#[test]
fn golden_update_allowlist() {
    let mut s = fresh();
    let id = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    // Add ALICE.
    let (rev, _) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::updateAllowlistCall { policyId: id, allowed: true, accounts: vec![ALICE] }
            .abi_encode(),
    );
    assert!(!rev);
    assert!(is_authorized(&mut s, id, ALICE));
    assert!(!is_authorized(&mut s, id, BOB));
    assert_eq!(
        s.get_events(registry()).last().unwrap().topics()[0],
        IPolicyRegistry::AllowlistUpdated::SIGNATURE_HASH
    );
    assert_root("update_allowlist", s, ROOT_UPDATE_ALLOWLIST);
}

#[test]
fn golden_update_allowlist_remove() {
    let mut s = fresh();
    let id = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    // Add then remove ALICE (exercises the membership-delete path).
    let (rev, _) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::updateAllowlistCall { policyId: id, allowed: true, accounts: vec![ALICE] }
            .abi_encode(),
    );
    assert!(!rev, "setup: add ALICE to allowlist must succeed");
    assert!(is_authorized(&mut s, id, ALICE));
    let (rev, _) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::updateAllowlistCall {
            policyId: id,
            allowed: false,
            accounts: vec![ALICE],
        }
        .abi_encode(),
    );
    assert!(!rev);
    assert!(!is_authorized(&mut s, id, ALICE));
    assert_root("update_allowlist_remove", s, ROOT_UPDATE_ALLOWLIST_REMOVE);
}

#[test]
fn golden_update_blocklist() {
    let mut s = fresh();
    let id = create(&mut s, ADMIN, ADMIN, PolicyType::BLOCKLIST);
    let (rev, _) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::updateBlocklistCall { policyId: id, blocked: true, accounts: vec![ALICE] }
            .abi_encode(),
    );
    assert!(!rev);
    // Blocked account rejected; others still authorized (empty-blocklist default).
    assert!(!is_authorized(&mut s, id, ALICE));
    assert!(is_authorized(&mut s, id, BOB));
    assert_eq!(
        s.get_events(registry()).last().unwrap().topics()[0],
        IPolicyRegistry::BlocklistUpdated::SIGNATURE_HASH
    );
    assert_root("update_blocklist", s, ROOT_UPDATE_BLOCKLIST);
}

#[test]
fn golden_update_allowlist_reverts_incompatible_type() {
    let mut s = fresh();
    let id = create(&mut s, ADMIN, ADMIN, PolicyType::BLOCKLIST);
    let (rev, bytes) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::updateAllowlistCall { policyId: id, allowed: true, accounts: vec![ALICE] }
            .abi_encode(),
    );
    assert!(rev);
    assert_eq!(bytes, Bytes::from(IPolicyRegistry::IncompatiblePolicyType {}.abi_encode()));
}

#[test]
fn golden_update_blocklist_reverts_incompatible_type() {
    let mut s = fresh();
    let id = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    let (rev, bytes) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::updateBlocklistCall { policyId: id, blocked: true, accounts: vec![ALICE] }
            .abi_encode(),
    );
    assert!(rev);
    assert_eq!(bytes, Bytes::from(IPolicyRegistry::IncompatiblePolicyType {}.abi_encode()));
}

#[test]
fn golden_update_allowlist_reverts_unauthorized() {
    let mut s = fresh();
    let id = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    let (rev, bytes) = call_policy(
        &mut s,
        OUTSIDER,
        IPolicyRegistry::updateAllowlistCall { policyId: id, allowed: true, accounts: vec![ALICE] }
            .abi_encode(),
    );
    assert!(rev);
    assert_eq!(bytes, Bytes::from(IPolicyRegistry::Unauthorized {}.abi_encode()));
}

#[test]
fn golden_update_allowlist_reverts_policy_not_found() {
    let mut s = fresh();
    // A well-formed ALLOWLIST id that was never created.
    let ghost = (1u64 << 56) | 42;
    let (rev, bytes) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::updateAllowlistCall {
            policyId: ghost,
            allowed: true,
            accounts: vec![ALICE],
        }
        .abi_encode(),
    );
    assert!(rev);
    assert_eq!(bytes, Bytes::from(IPolicyRegistry::PolicyNotFound {}.abi_encode()));
}

#[test]
fn golden_update_blocklist_reverts_batch_too_large() {
    let mut s = fresh();
    let id = create(&mut s, ADMIN, ADMIN, PolicyType::BLOCKLIST);
    let (rev, bytes) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::updateBlocklistCall {
            policyId: id,
            blocked: true,
            accounts: vec![ALICE; 65],
        }
        .abi_encode(),
    );
    assert!(rev);
    assert_eq!(
        bytes,
        Bytes::from(
            IPolicyRegistry::BatchSizeTooLarge { maxBatchSize: U256::from(64) }.abi_encode()
        )
    );
}

// ============================================================================
// admin lifecycle (stage / finalize / renounce)
// ============================================================================

#[test]
fn golden_stage_and_finalize_update_admin() {
    let mut s = fresh();
    let id = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    // Stage ADMIN2 as pending.
    let (rev, _) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::stageUpdateAdminCall { policyId: id, newAdmin: ADMIN2 }.abi_encode(),
    );
    assert!(!rev);
    let (_, pending) = call_policy(
        &mut s,
        OUTSIDER,
        IPolicyRegistry::pendingPolicyAdminCall { policyId: id }.abi_encode(),
    );
    assert_eq!(
        pending,
        Bytes::from(IPolicyRegistry::pendingPolicyAdminCall::abi_encode_returns(&ADMIN2))
    );
    assert_eq!(
        s.get_events(registry()).last().unwrap().topics()[0],
        IPolicyRegistry::PolicyAdminStaged::SIGNATURE_HASH
    );
    // Finalize by the pending admin.
    let (rev, _) = call_policy(
        &mut s,
        ADMIN2,
        IPolicyRegistry::finalizeUpdateAdminCall { policyId: id }.abi_encode(),
    );
    assert!(!rev);
    let (_, admin) = call_policy(
        &mut s,
        OUTSIDER,
        IPolicyRegistry::policyAdminCall { policyId: id }.abi_encode(),
    );
    assert_eq!(admin, Bytes::from(IPolicyRegistry::policyAdminCall::abi_encode_returns(&ADMIN2)));
    assert_eq!(
        s.get_events(registry()).last().unwrap().topics()[0],
        IPolicyRegistry::PolicyAdminUpdated::SIGNATURE_HASH
    );
    assert_root("stage_finalize_admin", s, ROOT_STAGE_FINALIZE_ADMIN);
}

#[test]
fn golden_stage_update_admin_reverts_unauthorized() {
    let mut s = fresh();
    let id = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    let (rev, bytes) = call_policy(
        &mut s,
        OUTSIDER,
        IPolicyRegistry::stageUpdateAdminCall { policyId: id, newAdmin: ADMIN2 }.abi_encode(),
    );
    assert!(rev);
    assert_eq!(bytes, Bytes::from(IPolicyRegistry::Unauthorized {}.abi_encode()));
}

#[test]
fn golden_finalize_reverts_no_pending() {
    let mut s = fresh();
    let id = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    let (rev, bytes) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::finalizeUpdateAdminCall { policyId: id }.abi_encode(),
    );
    assert!(rev);
    assert_eq!(bytes, Bytes::from(IPolicyRegistry::NoPendingAdmin {}.abi_encode()));
}

#[test]
fn golden_finalize_reverts_unauthorized() {
    let mut s = fresh();
    let id = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    let (rev, _) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::stageUpdateAdminCall { policyId: id, newAdmin: ADMIN2 }.abi_encode(),
    );
    assert!(!rev, "setup: staging pending admin must succeed");
    // A caller who is neither admin nor the pending admin.
    let (rev, bytes) = call_policy(
        &mut s,
        OUTSIDER,
        IPolicyRegistry::finalizeUpdateAdminCall { policyId: id }.abi_encode(),
    );
    assert!(rev);
    assert_eq!(bytes, Bytes::from(IPolicyRegistry::Unauthorized {}.abi_encode()));
}

#[test]
fn golden_renounce_admin() {
    let mut s = fresh();
    let id = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    let (rev, _) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::renounceAdminCall { policyId: id }.abi_encode(),
    );
    assert!(!rev);
    let (_, admin) = call_policy(
        &mut s,
        OUTSIDER,
        IPolicyRegistry::policyAdminCall { policyId: id }.abi_encode(),
    );
    assert_eq!(
        admin,
        Bytes::from(IPolicyRegistry::policyAdminCall::abi_encode_returns(&Address::ZERO))
    );
    assert_eq!(
        s.get_events(registry()).last().unwrap().topics()[0],
        IPolicyRegistry::PolicyAdminUpdated::SIGNATURE_HASH
    );
    assert_root("renounce_admin", s, ROOT_RENOUNCE_ADMIN);
}

#[test]
fn golden_renounce_admin_reverts_unauthorized() {
    let mut s = fresh();
    let id = create(&mut s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
    let (rev, bytes) = call_policy(
        &mut s,
        OUTSIDER,
        IPolicyRegistry::renounceAdminCall { policyId: id }.abi_encode(),
    );
    assert!(rev);
    assert_eq!(bytes, Bytes::from(IPolicyRegistry::Unauthorized {}.abi_encode()));
}

// ============================================================================
// dispatch envelope
// ============================================================================

#[test]
fn golden_reverts_nonzero_value() {
    let mut s = fresh();
    let calldata = IPolicyRegistry::isAuthorizedCall { policyId: 0, account: ALICE }.abi_encode();
    s.set_caller(ALICE);
    s.set_call_value(U256::ONE);
    let (rev, bytes) = StorageCtx::enter(&mut s, |ctx| {
        PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata, BaseUpgrade::Cobalt)
    })
    .map(|out| (out.is_revert(), out.bytes))
    .expect("dispatch must not fatally error");
    assert!(rev);
    assert_eq!(bytes, Bytes::from(IPolicyRegistry::NonPayable {}.abi_encode()));
}

#[test]
fn golden_write_reverts_when_not_activated() {
    // No activation: a write op must revert; a read op still works.
    let mut s = HashMapStorageProvider::new_with_storage_features(CHAIN_ID, StorageFeatures::Cobalt);
    let (rev, _) = call_policy(
        &mut s,
        ADMIN,
        IPolicyRegistry::createPolicyCall { admin: ADMIN, policyType: PolicyType::ALLOWLIST }
            .abi_encode(),
    );
    assert!(rev);
    // Reads bypass the activation gate.
    let (rev, bytes) = call_policy(
        &mut s,
        OUTSIDER,
        IPolicyRegistry::isAuthorizedCall { policyId: 0, account: ALICE }.abi_encode(),
    );
    assert!(!rev);
    assert_eq!(bytes, Bytes::from(IPolicyRegistry::isAuthorizedCall::abi_encode_returns(&true)));
}

// ============================================================================
// gas: storage-access footprint per op
// ============================================================================

/// Runs `calldata` as `caller` after `setup`, returning `(sload, sstore, keccak256)` counts.
fn gas(
    setup: impl FnOnce(&mut HashMapStorageProvider),
    caller: Address,
    calldata: Vec<u8>,
) -> (u64, u64, u64) {
    let mut s = fresh();
    setup(&mut s);
    s.set_caller(caller);
    s.reset_counters();
    StorageCtx::enter(&mut s, |ctx| {
        PolicyRegistryStorage::new(ctx).dispatch(ctx, &calldata, BaseUpgrade::Cobalt)
    })
    .expect("gas-footprint op must succeed");
    (s.counter_sload(), s.counter_sstore(), s.counter_keccak256())
}

#[test]
fn golden_gas_footprints() {
    let allow_id = ALLOWLIST_ID;
    let actual: Vec<(&str, (u64, u64, u64))> = vec![
        (
            "create_policy",
            gas(
                |_s| {},
                ADMIN,
                IPolicyRegistry::createPolicyCall {
                    admin: ADMIN,
                    policyType: PolicyType::ALLOWLIST,
                }
                .abi_encode(),
            ),
        ),
        (
            "update_allowlist",
            gas(
                |s| {
                    create(s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
                },
                ADMIN,
                IPolicyRegistry::updateAllowlistCall {
                    policyId: allow_id,
                    allowed: true,
                    accounts: vec![ALICE],
                }
                .abi_encode(),
            ),
        ),
        (
            "is_authorized",
            gas(
                |s| {
                    create(s, ADMIN, ADMIN, PolicyType::ALLOWLIST);
                },
                OUTSIDER,
                IPolicyRegistry::isAuthorizedCall { policyId: allow_id, account: ALICE }
                    .abi_encode(),
            ),
        ),
    ];

    let expected: &[(&str, (u64, u64, u64))] = &[
        ("create_policy", (2, 5, 0)),
        ("update_allowlist", (2, 1, 0)),
        ("is_authorized", (1, 0, 0)),
    ];

    bless_or_assert_gas(&actual, expected);
}

// ============================================================================
// meta: op coverage checklist
// ============================================================================

/// Compile-time coverage checklist — never called; its exhaustive `match` (no `_` arm) names the
/// golden `#[test]` fn(s) pinning each op. Adding an ABI op fails the build until a golden is added.
#[allow(dead_code)]
fn v2_op_coverage_checklist(call: IPolicyRegistry::IPolicyRegistryCalls) {
    use IPolicyRegistry::IPolicyRegistryCalls as C;

    fn covered(_goldens: &[fn()]) {}

    match call {
        C::createPolicy(_) => covered(&[
            golden_create_blocklist,
            golden_create_allowlist,
            golden_create_reverts_zero_admin,
            golden_create_policy_composite_type_is_incompatible_policy_type_in_v2,
        ]),
        C::createPolicyWithAccounts(_) => covered(&[
            golden_create_with_accounts,
            golden_create_with_accounts_blocklist,
            golden_create_with_accounts_reverts_batch_too_large,
        ]),
        C::updateAllowlist(_) => covered(&[
            golden_update_allowlist,
            golden_update_allowlist_remove,
            golden_update_allowlist_reverts_incompatible_type,
            golden_update_allowlist_reverts_unauthorized,
            golden_update_allowlist_reverts_policy_not_found,
        ]),
        C::updateBlocklist(_) => covered(&[
            golden_update_blocklist,
            golden_update_blocklist_reverts_incompatible_type,
            golden_update_blocklist_reverts_batch_too_large,
        ]),
        C::stageUpdateAdmin(_) => covered(&[
            golden_stage_and_finalize_update_admin,
            golden_stage_update_admin_reverts_unauthorized,
        ]),
        C::finalizeUpdateAdmin(_) => covered(&[
            golden_stage_and_finalize_update_admin,
            golden_finalize_reverts_no_pending,
            golden_finalize_reverts_unauthorized,
        ]),
        C::renounceAdmin(_) => {
            covered(&[golden_renounce_admin, golden_renounce_admin_reverts_unauthorized])
        }
        C::isAuthorized(_) => covered(&[
            golden_is_authorized_builtins_and_malformed,
            golden_is_authorized_empty_policies,
        ]),
        C::policyExists(_) => {
            covered(&[golden_reads_for_nonexistent_and_builtins, golden_create_blocklist])
        }
        C::policyAdmin(_) => {
            covered(&[golden_reads_for_nonexistent_and_builtins, golden_create_allowlist])
        }
        C::pendingPolicyAdmin(_) => covered(&[
            golden_reads_for_nonexistent_and_builtins,
            golden_stage_and_finalize_update_admin,
        ]),
        C::createCompositePolicy(_) => covered(&[
            golden_create_composite_union,
            golden_create_composite_reverts_incompatible_type,
        ]),
        C::updateComposite(_) => {
            covered(&[golden_update_composite, golden_update_composite_reverts_unauthorized])
        }
        C::compositePolicyChildIds(_) => covered(&[
            golden_composite_policy_child_ids,
            golden_composite_policy_child_ids_empty_for_non_composites,
            golden_composite_policy_child_ids_reads_before_activation,
        ]),
    }
}
