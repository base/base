//! Version 1 of the `PolicyRegistry` precompile logic, activated at Beryl.

use alloc::vec::Vec;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{
    IPolicyRegistry, IPolicyRegistry::PolicyType, PackedPolicy, PolicyAccounting,
    PolicyRegistryLogic,
};

/// First `PolicyRegistry` implementation. Frozen as of its activation at Beryl.
#[derive(Debug, Default, Clone, Copy)]
pub struct PolicyRegistryV1;

impl PolicyRegistryV1 {
    /// Built-in policy ID that always authorizes every account.
    ///
    /// Encoded as BLOCKLIST (type=0) with counter=0 — an empty blocklist authorizes
    /// everyone. Also the EVM zero default: zero-initialized policy ID fields map here.
    pub const ALWAYS_ALLOW_ID: u64 = 0;

    /// Built-in policy ID that always rejects every account.
    ///
    /// Encoded as ALLOWLIST (type=1) with counter=1 and an empty member set, so no account
    /// is on the allowlist and nobody passes.
    pub const ALWAYS_BLOCK_ID: u64 = (1u64 << Self::POLICY_ID_TYPE_SHIFT) | 1;

    /// Number of built-in policies; the counter lands here after initialization.
    pub const BUILTIN_POLICY_COUNT: u64 = 2;

    /// Mask covering the low 56 bits of a policy ID (the counter space).
    pub const COUNTER_MASK: u64 = (1u64 << 56) - 1;

    /// Maximum number of accounts per membership batch (`createPolicyWithAccounts`,
    /// `updateAllowlist`, `updateBlocklist`).
    pub const MAX_ACCOUNTS_PER_BATCH: usize = 64;

    const ALLOWLIST_TYPE: u8 = PolicyType::ALLOWLIST as u8;
    const BLOCKLIST_TYPE: u8 = PolicyType::BLOCKLIST as u8;
    const POLICY_ID_TYPE_SHIFT: usize = 56;

    /// Returns the policy type encoded in the top byte of `policy_id`.
    const fn policy_id_type(policy_id: u64) -> u8 {
        (policy_id >> Self::POLICY_ID_TYPE_SHIFT) as u8
    }

    /// Encodes a policy ID from its type discriminant and counter.
    pub const fn make_id(policy_type: u8, counter: u64) -> u64 {
        (policy_type as u64) << Self::POLICY_ID_TYPE_SHIFT | (counter & Self::COUNTER_MASK)
    }

    /// Reads a custom (non-built-in) policy word, reverting `PolicyNotFound` if absent.
    fn require_custom<S: PolicyAccounting>(
        &self,
        storage: &S,
        policy_id: u64,
    ) -> Result<PackedPolicy> {
        let packed = PackedPolicy::from_raw(storage.read_policy_word(policy_id)?);
        if !packed.exists() {
            return Err(BasePrecompileError::revert(IPolicyRegistry::PolicyNotFound {}));
        }
        Ok(packed)
    }

    /// Reverts `BatchSizeTooLarge` when a membership batch exceeds the limit.
    fn require_account_batch_size(accounts: &[Address]) -> Result<()> {
        if accounts.len() > Self::MAX_ACCOUNTS_PER_BATCH {
            return Err(BasePrecompileError::revert(IPolicyRegistry::BatchSizeTooLarge {
                maxBatchSize: U256::from(Self::MAX_ACCOUNTS_PER_BATCH),
            }));
        }
        Ok(())
    }

    /// Validates the policy exists and the caller is its current admin.
    /// Returns `(packed, caller)` on success.
    fn require_admin<S: PolicyAccounting>(
        &self,
        storage: &S,
        policy_id: u64,
    ) -> Result<(PackedPolicy, Address)> {
        let packed = self.require_custom(storage, policy_id)?;
        let caller = storage.caller();
        if packed.admin() != caller {
            return Err(BasePrecompileError::revert(IPolicyRegistry::Unauthorized {}));
        }
        Ok((packed, caller))
    }

    /// Validates policy-creation inputs and returns the raw policy type discriminator.
    fn validate_create_policy_inputs(admin: Address, policy_type: PolicyType) -> Result<u8> {
        if !policy_type.is_valid() {
            return Err(BasePrecompileError::enum_conversion_error());
        }
        if admin == Address::ZERO {
            return Err(BasePrecompileError::revert(IPolicyRegistry::ZeroAddress {}));
        }
        Ok(policy_type.as_discriminant())
    }

    /// First-touch setup for the registry: writes the bytecode marker and the two built-in
    /// policies, then leaves the counter at [`Self::BUILTIN_POLICY_COUNT`].
    ///
    /// Gated on the counter, so subsequent calls cost a single read and bail. The bytecode
    /// marker must precede any storage write because the EVM path can prune writes made
    /// under an empty native-precompile account. Kept inherent to V1 (off the trait) so it
    /// stays frozen with this version — it is an internal bootstrap primitive, not an ABI op.
    ///
    /// Both built-ins have a renounced (zero) admin:
    /// - [`Self::ALWAYS_ALLOW_ID`] (counter=0, BLOCKLIST): no members blocked — everyone authorized.
    /// - [`Self::ALWAYS_BLOCK_ID`] (counter=1, ALLOWLIST): no members allowed — nobody authorized.
    pub(crate) fn ensure_initialized_and_get_counter<S: PolicyAccounting>(
        &self,
        storage: &mut S,
    ) -> Result<u64> {
        let counter = storage.read_next_counter()?;
        if counter >= Self::BUILTIN_POLICY_COUNT {
            return Ok(counter);
        }
        // Assert that the ID constants match the enum discriminants and counter slots,
        // catching any future drift from enum reordering or constant changes.
        debug_assert_eq!(
            Self::make_id(PolicyType::BLOCKLIST.as_discriminant(), 0),
            Self::ALWAYS_ALLOW_ID
        );
        debug_assert_eq!(
            Self::make_id(PolicyType::ALLOWLIST.as_discriminant(), 1),
            Self::ALWAYS_BLOCK_ID
        );
        storage.mark_initialized()?;
        let builtin = PackedPolicy::new(Address::ZERO).into_u256();
        storage.write_policy_word(Self::ALWAYS_ALLOW_ID, builtin)?;
        storage.write_policy_word(Self::ALWAYS_BLOCK_ID, builtin)?;
        storage.write_next_counter(Self::BUILTIN_POLICY_COUNT)?;
        Ok(Self::BUILTIN_POLICY_COUNT)
    }

    /// Shared creation core after inputs have been validated.
    fn create_policy_inner<S: PolicyAccounting>(
        &self,
        storage: &mut S,
        admin: Address,
        policy_type: PolicyType,
        policy_type_u8: u8,
    ) -> Result<u64> {
        let counter = self.ensure_initialized_and_get_counter(storage)?;
        let is_counter_overflowed = counter >= Self::COUNTER_MASK;
        if is_counter_overflowed {
            return Err(BasePrecompileError::under_overflow());
        }
        storage.write_next_counter(counter + 1)?;
        let policy_id = Self::make_id(policy_type_u8, counter);
        storage.write_policy_word(policy_id, PackedPolicy::new(admin).into_u256())?;

        let caller = storage.caller();
        storage.emit_event(
            IPolicyRegistry::PolicyCreated {
                policyId: policy_id,
                creator: caller,
                policyType: policy_type,
            }
            .encode_log_data(),
        )?;
        storage.emit_event(
            IPolicyRegistry::PolicyAdminUpdated {
                policyId: policy_id,
                previousAdmin: Address::ZERO,
                newAdmin: admin,
            }
            .encode_log_data(),
        )?;

        Ok(policy_id)
    }

    /// Adds/removes `accounts` for `policy_id`, enforcing type, admin, and batch-size guards.
    /// Returns the caller on success.
    fn update_membership<S: PolicyAccounting>(
        &self,
        storage: &mut S,
        policy_id: u64,
        expected_type: u8,
        add: bool,
        accounts: &[Address],
    ) -> Result<Address> {
        // Check order matches Solidity canonical: existence → type → admin → batch size.
        let packed = self.require_custom(storage, policy_id)?;
        if Self::policy_id_type(policy_id) != expected_type {
            return Err(BasePrecompileError::revert(IPolicyRegistry::IncompatiblePolicyType {}));
        }
        let caller = storage.caller();
        if packed.admin() != caller {
            return Err(BasePrecompileError::revert(IPolicyRegistry::Unauthorized {}));
        }
        Self::require_account_batch_size(accounts)?;
        for account in accounts {
            if add {
                storage.set_member(policy_id, *account)?;
            } else {
                storage.delete_member(policy_id, *account)?;
            }
        }
        Ok(caller)
    }
}

impl<S: PolicyAccounting> PolicyRegistryLogic<S> for PolicyRegistryV1 {
    fn create_policy(
        &self,
        storage: &mut S,
        admin: Address,
        policy_type: PolicyType,
    ) -> Result<u64> {
        let policy_type_u8 = Self::validate_create_policy_inputs(admin, policy_type)?;
        self.create_policy_inner(storage, admin, policy_type, policy_type_u8)
    }

    fn create_policy_with_accounts(
        &self,
        storage: &mut S,
        admin: Address,
        policy_type: PolicyType,
        accounts: Vec<Address>,
    ) -> Result<u64> {
        let policy_type_u8 = Self::validate_create_policy_inputs(admin, policy_type)?;
        Self::require_account_batch_size(&accounts)?;
        let policy_id = self.create_policy_inner(storage, admin, policy_type, policy_type_u8)?;
        let caller = storage.caller();
        for account in &accounts {
            storage.set_member(policy_id, *account)?;
        }
        match policy_type {
            PolicyType::ALLOWLIST => storage.emit_event(
                IPolicyRegistry::AllowlistUpdated {
                    policyId: policy_id,
                    updater: caller,
                    allowed: true,
                    accounts,
                }
                .encode_log_data(),
            )?,
            PolicyType::BLOCKLIST => storage.emit_event(
                IPolicyRegistry::BlocklistUpdated {
                    policyId: policy_id,
                    updater: caller,
                    blocked: true,
                    accounts,
                }
                .encode_log_data(),
            )?,
            _ => return Err(BasePrecompileError::enum_conversion_error()),
        }
        Ok(policy_id)
    }

    fn stage_update_admin(
        &self,
        storage: &mut S,
        policy_id: u64,
        new_admin: Address,
    ) -> Result<()> {
        let (_, caller) = self.require_admin(storage, policy_id)?;
        if new_admin == Address::ZERO {
            storage.delete_pending_admin(policy_id)?;
        } else {
            storage.write_pending_admin(policy_id, new_admin)?;
        }
        storage.emit_event(
            IPolicyRegistry::PolicyAdminStaged {
                policyId: policy_id,
                currentAdmin: caller,
                pendingAdmin: new_admin,
            }
            .encode_log_data(),
        )?;
        Ok(())
    }

    fn finalize_update_admin(&self, storage: &mut S, policy_id: u64) -> Result<()> {
        let packed = self.require_custom(storage, policy_id)?;
        let pending = storage.read_pending_admin(policy_id)?;
        if pending == Address::ZERO {
            return Err(BasePrecompileError::revert(IPolicyRegistry::NoPendingAdmin {}));
        }
        let caller = storage.caller();
        if pending != caller {
            return Err(BasePrecompileError::revert(IPolicyRegistry::Unauthorized {}));
        }
        let previous_admin = packed.admin();
        storage.write_policy_word(policy_id, packed.with_admin(caller).into_u256())?;
        storage.delete_pending_admin(policy_id)?;
        storage.emit_event(
            IPolicyRegistry::PolicyAdminUpdated {
                policyId: policy_id,
                previousAdmin: previous_admin,
                newAdmin: caller,
            }
            .encode_log_data(),
        )?;
        Ok(())
    }

    fn renounce_admin(&self, storage: &mut S, policy_id: u64) -> Result<()> {
        let (packed, caller) = self.require_admin(storage, policy_id)?;
        storage.write_policy_word(policy_id, packed.with_admin(Address::ZERO).into_u256())?;
        storage.delete_pending_admin(policy_id)?;
        storage.emit_event(
            IPolicyRegistry::PolicyAdminUpdated {
                policyId: policy_id,
                previousAdmin: caller,
                newAdmin: Address::ZERO,
            }
            .encode_log_data(),
        )?;
        Ok(())
    }

    fn update_allowlist(
        &self,
        storage: &mut S,
        policy_id: u64,
        allowed: bool,
        accounts: Vec<Address>,
    ) -> Result<()> {
        let caller =
            self.update_membership(storage, policy_id, Self::ALLOWLIST_TYPE, allowed, &accounts)?;
        storage.emit_event(
            IPolicyRegistry::AllowlistUpdated {
                policyId: policy_id,
                updater: caller,
                allowed,
                accounts,
            }
            .encode_log_data(),
        )
    }

    fn update_blocklist(
        &self,
        storage: &mut S,
        policy_id: u64,
        blocked: bool,
        accounts: Vec<Address>,
    ) -> Result<()> {
        let caller =
            self.update_membership(storage, policy_id, Self::BLOCKLIST_TYPE, blocked, &accounts)?;
        storage.emit_event(
            IPolicyRegistry::BlocklistUpdated {
                policyId: policy_id,
                updater: caller,
                blocked,
                accounts,
            }
            .encode_log_data(),
        )
    }

    fn is_authorized(&self, storage: &S, policy_id: u64, account: Address) -> Result<bool> {
        // Malformed IDs (type byte > 1) are treated as unauthorized rather than reverting.
        if Self::policy_id_type(policy_id) > PolicyType::ALLOWLIST as u8 {
            return Ok(false);
        }
        // Fast-paths for built-in IDs: ALWAYS_ALLOW_ID = 0 is the EVM default for any
        // uninitialized policy field, so this must work before initialization has run.
        if policy_id == Self::ALWAYS_ALLOW_ID {
            return Ok(true);
        }
        if policy_id == Self::ALWAYS_BLOCK_ID {
            return Ok(false);
        }
        // Read membership directly without requiring the policy slot to be written first.
        // An unwritten slot returns false, which naturally gives:
        //   ALLOWLIST  => false  (no members => not authorized)
        //   BLOCKLIST  => !false (no members blocked => authorized)
        let member = storage.read_member(policy_id, account)?;
        match Self::policy_id_type(policy_id) {
            Self::ALLOWLIST_TYPE => Ok(member),
            Self::BLOCKLIST_TYPE => Ok(!member),
            _ => unreachable!("type byte > 1 was rejected by the malformed-ID guard above"),
        }
    }

    fn policy_exists(&self, storage: &S, policy_id: u64) -> Result<bool> {
        // Malformed IDs (type byte > 1) are not well-formed, so they do not exist.
        if Self::policy_id_type(policy_id) > PolicyType::ALLOWLIST as u8 {
            return Ok(false);
        }
        if policy_id == Self::ALWAYS_ALLOW_ID || policy_id == Self::ALWAYS_BLOCK_ID {
            return Ok(true);
        }
        let packed = PackedPolicy::from_raw(storage.read_policy_word(policy_id)?);
        Ok(packed.exists())
    }

    fn get_policy_admin(&self, storage: &S, policy_id: u64) -> Result<Address> {
        if Self::policy_id_type(policy_id) > PolicyType::ALLOWLIST as u8 {
            return Ok(Address::ZERO);
        }
        let packed = PackedPolicy::from_raw(storage.read_policy_word(policy_id)?);
        if !packed.exists() {
            return Ok(Address::ZERO);
        }
        Ok(packed.admin())
    }

    fn pending_policy_admin(&self, storage: &S, policy_id: u64) -> Result<Address> {
        if Self::policy_id_type(policy_id) > PolicyType::ALLOWLIST as u8 {
            return Ok(Address::ZERO);
        }
        if policy_id == Self::ALWAYS_ALLOW_ID || policy_id == Self::ALWAYS_BLOCK_ID {
            return Ok(Address::ZERO);
        }
        storage.read_pending_admin(policy_id)
    }
}

#[cfg(test)]
mod tests {
    use alloc::{collections::BTreeMap, vec, vec::Vec};

    use alloy_primitives::{Address, LogData, U256, address};
    use alloy_sol_types::SolEvent;
    use base_precompile_storage::{BasePrecompileError, Result};

    use crate::{
        IPolicyRegistry, IPolicyRegistry::PolicyType, PolicyAccounting, PolicyRegistryLogic,
        PolicyRegistryV1,
    };

    const REGISTRY: Address = address!("0x8453000000000000000000000000000000000002");
    const ADMIN: Address = address!("0x1000000000000000000000000000000000000001");
    const ALICE: Address = address!("0xA000000000000000000000000000000000000001");
    const BOB: Address = address!("0xB000000000000000000000000000000000000001");
    const NEW_ADMIN: Address = address!("0x2000000000000000000000000000000000000002");
    const LOGIC: PolicyRegistryV1 = PolicyRegistryV1;

    // --- Self-contained in-memory fake (no dependency on `common::test_utils`, so shared
    //     test scaffolding can never drift this frozen version's coverage) ---

    /// Minimal [`PolicyAccounting`] backed by in-memory maps. `delete_*` removes the key so
    /// its read semantics match `Mapping::delete` (a zeroed slot), not a written zero value.
    #[derive(Debug)]
    struct FakePolicyAccounting {
        caller: Address,
        initialized: bool,
        policies: BTreeMap<u64, U256>,
        members: BTreeMap<(u64, Address), bool>,
        pending_admins: BTreeMap<u64, Address>,
        next_counter: u64,
        events: Vec<LogData>,
    }

    impl FakePolicyAccounting {
        fn new() -> Self {
            Self {
                caller: ADMIN,
                initialized: false,
                policies: BTreeMap::new(),
                members: BTreeMap::new(),
                pending_admins: BTreeMap::new(),
                next_counter: 0,
                events: Vec::new(),
            }
        }
    }

    impl PolicyAccounting for FakePolicyAccounting {
        fn registry_address(&self) -> Address {
            REGISTRY
        }
        fn caller(&self) -> Address {
            self.caller
        }
        fn read_policy_word(&self, policy_id: u64) -> Result<U256> {
            Ok(self.policies.get(&policy_id).copied().unwrap_or(U256::ZERO))
        }
        fn write_policy_word(&mut self, policy_id: u64, word: U256) -> Result<()> {
            self.policies.insert(policy_id, word);
            Ok(())
        }
        fn read_member(&self, policy_id: u64, account: Address) -> Result<bool> {
            Ok(self.members.get(&(policy_id, account)).copied().unwrap_or(false))
        }
        fn set_member(&mut self, policy_id: u64, account: Address) -> Result<()> {
            self.members.insert((policy_id, account), true);
            Ok(())
        }
        fn delete_member(&mut self, policy_id: u64, account: Address) -> Result<()> {
            self.members.remove(&(policy_id, account));
            Ok(())
        }
        fn read_pending_admin(&self, policy_id: u64) -> Result<Address> {
            Ok(self.pending_admins.get(&policy_id).copied().unwrap_or(Address::ZERO))
        }
        fn write_pending_admin(&mut self, policy_id: u64, admin: Address) -> Result<()> {
            self.pending_admins.insert(policy_id, admin);
            Ok(())
        }
        fn delete_pending_admin(&mut self, policy_id: u64) -> Result<()> {
            self.pending_admins.remove(&policy_id);
            Ok(())
        }
        fn read_next_counter(&self) -> Result<u64> {
            Ok(self.next_counter)
        }
        fn write_next_counter(&mut self, counter: u64) -> Result<()> {
            self.next_counter = counter;
            Ok(())
        }
        fn emit_event(&mut self, log: LogData) -> Result<()> {
            self.events.push(log);
            Ok(())
        }
        fn mark_initialized(&mut self) -> Result<()> {
            self.initialized = true;
            Ok(())
        }
    }

    type Storage = FakePolicyAccounting;

    /// Bare storage (no built-ins seeded), caller = `ADMIN`.
    fn bare() -> Storage {
        FakePolicyAccounting::new()
    }

    /// Storage with both built-in policies seeded and the counter at 2.
    fn initialized() -> Storage {
        let mut storage = bare();
        LOGIC.ensure_initialized_and_get_counter(&mut storage).unwrap();
        storage
    }

    fn set_caller(storage: &mut Storage, caller: Address) {
        storage.caller = caller;
    }

    fn create_allowlist(storage: &mut Storage) -> u64 {
        set_caller(storage, ADMIN);
        LOGIC.create_policy(storage, ADMIN, PolicyType::ALLOWLIST).unwrap()
    }

    fn create_blocklist(storage: &mut Storage) -> u64 {
        set_caller(storage, ADMIN);
        LOGIC.create_policy(storage, ADMIN, PolicyType::BLOCKLIST).unwrap()
    }

    fn is_authorized(storage: &Storage, policy_id: u64, account: Address) -> bool {
        LOGIC.is_authorized(storage, policy_id, account).unwrap()
    }

    fn many_accounts(count: usize) -> Vec<Address> {
        (0..count).map(|i| Address::from_word(U256::from(i as u64 + 1).into())).collect()
    }

    // --- built-in IDs ---

    #[test]
    fn always_allow_id_authorizes_any_account() {
        let rt = initialized();
        assert!(is_authorized(&rt, PolicyRegistryV1::ALWAYS_ALLOW_ID, ALICE));
        assert!(is_authorized(&rt, PolicyRegistryV1::ALWAYS_ALLOW_ID, BOB));
    }

    #[test]
    fn always_block_id_rejects_any_account() {
        let rt = initialized();
        assert!(!is_authorized(&rt, PolicyRegistryV1::ALWAYS_BLOCK_ID, ALICE));
        assert!(!is_authorized(&rt, PolicyRegistryV1::ALWAYS_BLOCK_ID, BOB));
    }

    #[test]
    fn unknown_blocklist_policy_id_authorizes_account() {
        // 0xdeadbeef has type byte 0 (BLOCKLIST); no members blocked => authorized.
        let rt = initialized();
        assert!(is_authorized(&rt, 0xdeadbeef, ALICE));
    }

    #[test]
    fn unknown_allowlist_policy_id_does_not_authorize_account() {
        let unknown_allowlist = PolicyRegistryV1::make_id(PolicyType::ALLOWLIST as u8, 9999);
        let rt = initialized();
        assert!(!is_authorized(&rt, unknown_allowlist, ALICE));
    }

    #[test]
    fn malformed_policy_id_is_authorized_returns_false() {
        let malformed: u64 = (2u64 << 56) | 42;
        let rt = initialized();
        assert!(!LOGIC.is_authorized(&rt, malformed, ALICE).unwrap());
    }

    #[test]
    fn malformed_policy_id_policy_exists_returns_false() {
        let malformed: u64 = (5u64 << 56) | 100;
        let rt = initialized();
        assert!(!LOGIC.policy_exists(&rt, malformed).unwrap());
    }

    // --- ensure_initialized_and_get_counter ---

    #[test]
    fn first_create_policy_initializes_builtins_and_starts_counter_at_two() {
        let mut rt = bare();
        let id = LOGIC.create_policy(&mut rt, ADMIN, PolicyType::ALLOWLIST).unwrap();
        assert_eq!(id & PolicyRegistryV1::COUNTER_MASK, 2);
        assert!(LOGIC.policy_exists(&rt, PolicyRegistryV1::ALWAYS_ALLOW_ID).unwrap());
        assert!(LOGIC.policy_exists(&rt, PolicyRegistryV1::ALWAYS_BLOCK_ID).unwrap());
        assert!(rt.initialized);
    }

    #[test]
    fn ensure_initialized_and_get_counter_is_idempotent() {
        let mut rt = bare();
        for _ in 0..3 {
            LOGIC.ensure_initialized_and_get_counter(&mut rt).unwrap();
        }
        assert_eq!(rt.next_counter, PolicyRegistryV1::BUILTIN_POLICY_COUNT);
    }

    // --- createPolicy ---

    #[test]
    fn create_policy_zero_admin_reverts() {
        let mut rt = initialized();
        let err = LOGIC.create_policy(&mut rt, Address::ZERO, PolicyType::ALLOWLIST).unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(IPolicyRegistry::ZeroAddress {}));
    }

    #[test]
    fn create_policy_ids_encode_type_in_top_byte_and_increment_counter() {
        let mut rt = initialized();
        let id1 = create_allowlist(&mut rt);
        let id2 = create_blocklist(&mut rt);
        assert_eq!((id1 >> 56) as u8, PolicyType::ALLOWLIST as u8);
        assert_eq!((id2 >> 56) as u8, PolicyType::BLOCKLIST as u8);
        assert_eq!(id1 & PolicyRegistryV1::COUNTER_MASK, 2);
        assert_eq!(id2 & PolicyRegistryV1::COUNTER_MASK, 3);
    }

    #[test]
    fn create_policy_at_counter_mask_reverts_with_under_overflow() {
        let mut rt = initialized();
        rt.next_counter = PolicyRegistryV1::COUNTER_MASK;
        let err = LOGIC.create_policy(&mut rt, ADMIN, PolicyType::ALLOWLIST).unwrap_err();
        assert_eq!(err, BasePrecompileError::under_overflow());
    }

    #[test]
    fn create_policy_at_counter_mask_minus_one_consumes_last_slot_then_reverts() {
        let mut rt = initialized();
        rt.next_counter = PolicyRegistryV1::COUNTER_MASK - 1;
        let id = LOGIC.create_policy(&mut rt, ADMIN, PolicyType::ALLOWLIST).unwrap();
        assert_eq!(id & PolicyRegistryV1::COUNTER_MASK, PolicyRegistryV1::COUNTER_MASK - 1);
        let err = LOGIC.create_policy(&mut rt, ADMIN, PolicyType::ALLOWLIST).unwrap_err();
        assert_eq!(err, BasePrecompileError::under_overflow());
    }

    #[test]
    fn create_policy_emits_policy_created_and_admin_updated_events() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        let events = &rt.events;
        assert_eq!(events.len(), 2);
        let created = IPolicyRegistry::PolicyCreated::decode_log_data(&events[0]).unwrap();
        assert_eq!(created.policyId, id);
        assert_eq!(created.creator, ADMIN);
        assert_eq!(created.policyType, PolicyType::ALLOWLIST);
    }

    #[test]
    fn update_allowlist_emits_allowlist_updated_event() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        LOGIC.update_allowlist(&mut rt, id, true, vec![ALICE]).unwrap();
        let updated =
            IPolicyRegistry::AllowlistUpdated::decode_log_data(rt.events.last().unwrap()).unwrap();
        assert_eq!(updated.policyId, id);
        assert_eq!(updated.updater, ADMIN);
        assert!(updated.allowed);
        assert_eq!(updated.accounts, vec![ALICE]);
    }

    // --- ALLOWLIST membership ---

    #[test]
    fn allowlist_non_member_is_not_authorized() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        assert!(!is_authorized(&rt, id, ALICE));
    }

    #[test]
    fn allowlist_add_then_remove_member() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        LOGIC.update_allowlist(&mut rt, id, true, vec![ALICE]).unwrap();
        assert!(is_authorized(&rt, id, ALICE));
        LOGIC.update_allowlist(&mut rt, id, false, vec![ALICE]).unwrap();
        assert!(!is_authorized(&rt, id, ALICE));
    }

    #[test]
    fn allowlist_batch_update_flips_all_accounts() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        LOGIC.update_allowlist(&mut rt, id, true, vec![ALICE, BOB]).unwrap();
        assert!(is_authorized(&rt, id, ALICE));
        assert!(is_authorized(&rt, id, BOB));
        LOGIC.update_allowlist(&mut rt, id, false, vec![ALICE, BOB]).unwrap();
        assert!(!is_authorized(&rt, id, ALICE));
        assert!(!is_authorized(&rt, id, BOB));
    }

    #[test]
    fn update_allowlist_too_many_accounts_reverts() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        let accounts = many_accounts(PolicyRegistryV1::MAX_ACCOUNTS_PER_BATCH + 1);
        let err = LOGIC.update_allowlist(&mut rt, id, true, accounts).unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IPolicyRegistry::BatchSizeTooLarge {
                maxBatchSize: U256::from(PolicyRegistryV1::MAX_ACCOUNTS_PER_BATCH),
            })
        );
    }

    #[test]
    fn update_allowlist_max_batch_size_succeeds() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        let accounts = many_accounts(PolicyRegistryV1::MAX_ACCOUNTS_PER_BATCH);
        LOGIC.update_allowlist(&mut rt, id, true, accounts).unwrap();
    }

    #[test]
    fn allowlist_readding_existing_member_is_idempotent() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        for _ in 0..2 {
            LOGIC.update_allowlist(&mut rt, id, true, vec![ALICE]).unwrap();
        }
        assert!(is_authorized(&rt, id, ALICE));
    }

    #[test]
    fn allowlist_removing_non_member_is_idempotent() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        LOGIC.update_allowlist(&mut rt, id, false, vec![ALICE]).unwrap();
        assert!(!is_authorized(&rt, id, ALICE));
    }

    #[test]
    fn update_allowlist_on_blocklist_policy_reverts() {
        let mut rt = initialized();
        let id = create_blocklist(&mut rt);
        let err = LOGIC.update_allowlist(&mut rt, id, true, vec![ALICE]).unwrap_err();
        assert!(matches!(err, BasePrecompileError::Revert(_)));
    }

    // --- BLOCKLIST membership ---

    #[test]
    fn blocklist_non_member_is_authorized() {
        let mut rt = initialized();
        let id = create_blocklist(&mut rt);
        assert!(is_authorized(&rt, id, ALICE));
    }

    #[test]
    fn blocklist_block_then_unblock_member() {
        let mut rt = initialized();
        let id = create_blocklist(&mut rt);
        LOGIC.update_blocklist(&mut rt, id, true, vec![ALICE]).unwrap();
        assert!(!is_authorized(&rt, id, ALICE));
        LOGIC.update_blocklist(&mut rt, id, false, vec![ALICE]).unwrap();
        assert!(is_authorized(&rt, id, ALICE));
    }

    #[test]
    fn update_blocklist_on_allowlist_policy_reverts() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        let err = LOGIC.update_blocklist(&mut rt, id, true, vec![ALICE]).unwrap_err();
        assert!(matches!(err, BasePrecompileError::Revert(_)));
    }

    #[test]
    fn update_blocklist_too_many_accounts_reverts() {
        let mut rt = initialized();
        let id = create_blocklist(&mut rt);
        let accounts = many_accounts(PolicyRegistryV1::MAX_ACCOUNTS_PER_BATCH + 1);
        let err = LOGIC.update_blocklist(&mut rt, id, true, accounts).unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IPolicyRegistry::BatchSizeTooLarge {
                maxBatchSize: U256::from(PolicyRegistryV1::MAX_ACCOUNTS_PER_BATCH),
            })
        );
    }

    #[test]
    fn update_allowlist_on_blocklist_policy_by_non_admin_reverts_with_incompatible_type() {
        let mut rt = initialized();
        let id = create_blocklist(&mut rt);
        set_caller(&mut rt, ALICE);
        let err = LOGIC.update_allowlist(&mut rt, id, true, vec![BOB]).unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(IPolicyRegistry::IncompatiblePolicyType {}));
    }

    #[test]
    fn update_blocklist_on_allowlist_policy_by_non_admin_reverts_with_incompatible_type() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        set_caller(&mut rt, ALICE);
        let err = LOGIC.update_blocklist(&mut rt, id, true, vec![BOB]).unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(IPolicyRegistry::IncompatiblePolicyType {}));
    }

    // --- createPolicyWithAccounts ---

    #[test]
    fn create_policy_with_accounts_seeds_members() {
        let mut rt = initialized();
        let id = LOGIC
            .create_policy_with_accounts(&mut rt, ADMIN, PolicyType::ALLOWLIST, vec![ALICE, BOB])
            .unwrap();
        assert!(is_authorized(&rt, id, ALICE));
        assert!(is_authorized(&rt, id, BOB));
    }

    #[test]
    fn create_policy_with_accounts_empty_batch_emits_seed_event() {
        let mut rt = initialized();
        let id = LOGIC
            .create_policy_with_accounts(&mut rt, ADMIN, PolicyType::ALLOWLIST, Vec::new())
            .unwrap();
        let events = &rt.events;
        assert_eq!(events.len(), 3);
        let updated =
            IPolicyRegistry::AllowlistUpdated::decode_log_data(events.last().unwrap()).unwrap();
        assert_eq!(updated.policyId, id);
        assert_eq!(updated.updater, ADMIN);
        assert!(updated.allowed);
        assert!(updated.accounts.is_empty());
    }

    #[test]
    fn create_policy_with_accounts_zero_account_is_seeded() {
        let mut rt = initialized();
        let id = LOGIC
            .create_policy_with_accounts(
                &mut rt,
                ADMIN,
                PolicyType::ALLOWLIST,
                vec![ALICE, Address::ZERO],
            )
            .unwrap();
        assert!(LOGIC.is_authorized(&rt, id, Address::ZERO).unwrap());
    }

    #[test]
    fn create_policy_with_accounts_too_many_accounts_reverts() {
        let mut rt = initialized();
        let accounts = many_accounts(PolicyRegistryV1::MAX_ACCOUNTS_PER_BATCH + 1);
        let err = LOGIC
            .create_policy_with_accounts(&mut rt, ADMIN, PolicyType::ALLOWLIST, accounts)
            .unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IPolicyRegistry::BatchSizeTooLarge {
                maxBatchSize: U256::from(PolicyRegistryV1::MAX_ACCOUNTS_PER_BATCH),
            })
        );
    }

    #[test]
    fn create_policy_with_accounts_zero_admin_precedes_batch_size_revert() {
        let mut rt = initialized();
        let accounts = many_accounts(PolicyRegistryV1::MAX_ACCOUNTS_PER_BATCH + 1);
        let err = LOGIC
            .create_policy_with_accounts(&mut rt, Address::ZERO, PolicyType::ALLOWLIST, accounts)
            .unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(IPolicyRegistry::ZeroAddress {}));
    }

    #[test]
    fn create_policy_with_accounts_invalid_policy_type_precedes_batch_size_revert() {
        let mut rt = initialized();
        let accounts = many_accounts(PolicyRegistryV1::MAX_ACCOUNTS_PER_BATCH + 1);
        let err = LOGIC
            .create_policy_with_accounts(&mut rt, ADMIN, PolicyType::__Invalid, accounts)
            .unwrap_err();
        assert_eq!(err, BasePrecompileError::enum_conversion_error());
    }

    #[test]
    fn create_policy_with_accounts_blocklist_seeds_blocked_members() {
        let mut rt = initialized();
        let id = LOGIC
            .create_policy_with_accounts(&mut rt, ADMIN, PolicyType::BLOCKLIST, vec![ALICE, BOB])
            .unwrap();
        assert!(!is_authorized(&rt, id, ALICE));
        assert!(!is_authorized(&rt, id, BOB));
    }

    // --- two-step admin transfer ---

    #[test]
    fn admin_transfer_two_step() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        LOGIC.stage_update_admin(&mut rt, id, NEW_ADMIN).unwrap();
        set_caller(&mut rt, NEW_ADMIN);
        LOGIC.finalize_update_admin(&mut rt, id).unwrap();
        LOGIC.update_allowlist(&mut rt, id, true, vec![ALICE]).unwrap();
        assert!(is_authorized(&rt, id, ALICE));
        assert_eq!(LOGIC.get_policy_admin(&rt, id).unwrap(), NEW_ADMIN);
    }

    #[test]
    fn finalize_update_admin_without_pending_reverts() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        let err = LOGIC.finalize_update_admin(&mut rt, id).unwrap_err();
        assert!(matches!(err, BasePrecompileError::Revert(_)));
    }

    #[test]
    fn stage_update_admin_unauthorized_reverts() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        set_caller(&mut rt, ALICE);
        let err = LOGIC.stage_update_admin(&mut rt, id, NEW_ADMIN).unwrap_err();
        assert!(matches!(err, BasePrecompileError::Revert(_)));
    }

    #[test]
    fn finalize_update_admin_unauthorized_reverts() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        LOGIC.stage_update_admin(&mut rt, id, NEW_ADMIN).unwrap();
        set_caller(&mut rt, ALICE);
        let err = LOGIC.finalize_update_admin(&mut rt, id).unwrap_err();
        assert!(matches!(err, BasePrecompileError::Revert(_)));
    }

    // --- renounceAdmin ---

    #[test]
    fn renounce_admin_freezes_policy() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        LOGIC.renounce_admin(&mut rt, id).unwrap();
        let err = LOGIC.update_allowlist(&mut rt, id, true, vec![ALICE]).unwrap_err();
        assert!(matches!(err, BasePrecompileError::Revert(_)));
    }

    #[test]
    fn renounce_admin_unauthorized_reverts() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        set_caller(&mut rt, ALICE);
        let err = LOGIC.renounce_admin(&mut rt, id).unwrap_err();
        assert!(matches!(err, BasePrecompileError::Revert(_)));
    }

    #[test]
    fn builtin_policies_reject_admin_mutations() {
        let mut rt = initialized();
        for policy_id in [PolicyRegistryV1::ALWAYS_ALLOW_ID, PolicyRegistryV1::ALWAYS_BLOCK_ID] {
            let err = LOGIC.stage_update_admin(&mut rt, policy_id, ALICE).unwrap_err();
            assert!(matches!(err, BasePrecompileError::Revert(_)));
        }
    }

    // --- read helpers for built-in / unknown / malformed IDs ---

    #[test]
    fn policy_exists_builtin_ids_always_return_true() {
        let rt = bare();
        assert!(LOGIC.policy_exists(&rt, PolicyRegistryV1::ALWAYS_ALLOW_ID).unwrap());
        assert!(LOGIC.policy_exists(&rt, PolicyRegistryV1::ALWAYS_BLOCK_ID).unwrap());
    }

    #[test]
    fn get_policy_admin_builtin_ids_return_zero_address() {
        let rt = initialized();
        assert_eq!(
            LOGIC.get_policy_admin(&rt, PolicyRegistryV1::ALWAYS_ALLOW_ID).unwrap(),
            Address::ZERO
        );
        assert_eq!(
            LOGIC.get_policy_admin(&rt, PolicyRegistryV1::ALWAYS_BLOCK_ID).unwrap(),
            Address::ZERO
        );
    }

    #[test]
    fn get_policy_admin_malformed_policy_id_returns_zero_address() {
        let rt = initialized();
        let malformed: u64 = (2u64 << 56) | 42;
        assert_eq!(LOGIC.get_policy_admin(&rt, malformed).unwrap(), Address::ZERO);
    }

    #[test]
    fn get_policy_admin_nonexistent_policy_returns_zero_address() {
        let rt = initialized();
        assert_eq!(LOGIC.get_policy_admin(&rt, 0xdeadbeef).unwrap(), Address::ZERO);
    }

    #[test]
    fn pending_policy_admin_builtin_ids_return_zero_address() {
        let rt = initialized();
        assert_eq!(
            LOGIC.pending_policy_admin(&rt, PolicyRegistryV1::ALWAYS_ALLOW_ID).unwrap(),
            Address::ZERO
        );
        assert_eq!(
            LOGIC.pending_policy_admin(&rt, PolicyRegistryV1::ALWAYS_BLOCK_ID).unwrap(),
            Address::ZERO
        );
    }

    #[test]
    fn pending_policy_admin_builtin_ids_short_circuit_staged_slot() {
        let mut rt = initialized();
        for policy_id in [PolicyRegistryV1::ALWAYS_ALLOW_ID, PolicyRegistryV1::ALWAYS_BLOCK_ID] {
            rt.pending_admins.insert(policy_id, NEW_ADMIN);
            assert_eq!(
                LOGIC.pending_policy_admin(&rt, policy_id).unwrap(),
                Address::ZERO,
                "built-in policy {policy_id} must ignore a staged pending slot"
            );
        }
    }

    #[test]
    fn pending_policy_admin_counter_one_blocklist_reads_staged_slot() {
        // BLOCKLIST counter=1 is not ALWAYS_BLOCK_ID, which is ALLOWLIST counter=1.
        let counter_one_blocklist = PolicyRegistryV1::make_id(PolicyType::BLOCKLIST as u8, 1);
        assert_ne!(counter_one_blocklist, PolicyRegistryV1::ALWAYS_BLOCK_ID);
        let mut rt = initialized();
        rt.pending_admins.insert(counter_one_blocklist, NEW_ADMIN);
        assert_eq!(LOGIC.pending_policy_admin(&rt, counter_one_blocklist).unwrap(), NEW_ADMIN);
    }

    #[test]
    fn pending_policy_admin_unknown_id_returns_zero_address() {
        let rt = initialized();
        assert_eq!(LOGIC.pending_policy_admin(&rt, 0xdeadbeef).unwrap(), Address::ZERO);
    }

    #[test]
    fn pending_policy_admin_malformed_policy_id_returns_zero_address() {
        let rt = initialized();
        let malformed: u64 = (2u64 << 56) | 42;
        assert_eq!(LOGIC.pending_policy_admin(&rt, malformed).unwrap(), Address::ZERO);
    }

    #[test]
    fn pending_policy_admin_nonexistent_well_formed_policy_returns_zero_address() {
        let rt = initialized();
        let nonexistent = PolicyRegistryV1::make_id(0, 999);
        assert_eq!(LOGIC.pending_policy_admin(&rt, nonexistent).unwrap(), Address::ZERO);
    }
}
