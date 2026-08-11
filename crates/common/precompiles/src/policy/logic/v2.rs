//! Version 2 of the `PolicyRegistry` precompile logic, activated at Cobalt.

use alloc::vec::Vec;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{
    IPolicyRegistry, IPolicyRegistry::PolicyType, PackedPolicy, PolicyAccounting,
    PolicyRegistryLogic,
};

/// Second `PolicyRegistry` implementation. Activated at Cobalt, behavior-identical to V1 (scaffold seam for future changes).
#[derive(Debug, Default, Clone, Copy)]
pub struct PolicyRegistryV2;

impl PolicyRegistryV2 {
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

    /// Minimum number of child policies a composite must reference.
    pub const MIN_CHILD_POLICIES: usize = 2;

    /// Maximum number of child policies a composite may reference. Distinct from
    /// [`Self::MAX_ACCOUNTS_PER_BATCH`], which caps account-membership batches.
    pub const MAX_CHILD_POLICIES: usize = 4;

    const ALLOWLIST_TYPE: u8 = PolicyType::ALLOWLIST as u8;
    const BLOCKLIST_TYPE: u8 = PolicyType::BLOCKLIST as u8;
    const UNION_TYPE: u8 = PolicyType::UNION as u8;
    const INTERSECT_TYPE: u8 = PolicyType::INTERSECT as u8;
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
    fn require_existing_policy<S: PolicyAccounting>(
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
        let packed = self.require_existing_policy(storage, policy_id)?;
        let caller = storage.caller();
        if packed.admin() != caller {
            return Err(BasePrecompileError::revert(IPolicyRegistry::Unauthorized {}));
        }
        Ok((packed, caller))
    }

    /// Returns whether `policy_id`'s top byte encodes a composite gate (UNION or INTERSECT).
    const fn is_composite(policy_id: u64) -> bool {
        let policy_type = Self::policy_id_type(policy_id);
        policy_type == Self::UNION_TYPE || policy_type == Self::INTERSECT_TYPE
    }

    /// Returns whether `policy_id`'s top byte is a supported policy type (simple or composite).
    /// Type bytes above INTERSECT are malformed.
    const fn is_well_formed(policy_id: u64) -> bool {
        Self::policy_id_type(policy_id) <= Self::INTERSECT_TYPE
    }

    /// Returns whether `policy_id` is a built-in sentinel (`ALWAYS_ALLOW` / `ALWAYS_BLOCK`).
    const fn is_builtin(policy_id: u64) -> bool {
        policy_id == Self::ALWAYS_ALLOW_ID || policy_id == Self::ALWAYS_BLOCK_ID
    }

    /// Reverts `ChildPoliciesOutsideOfRange` when the child count is outside `[2, 4]`.
    fn require_child_policy_in_range(child_policy_ids: &[u64]) -> Result<()> {
        let count = child_policy_ids.len();
        if !(Self::MIN_CHILD_POLICIES..=Self::MAX_CHILD_POLICIES).contains(&count) {
            return Err(BasePrecompileError::revert(
                IPolicyRegistry::ChildPoliciesOutsideOfRange {},
            ));
        }
        Ok(())
    }

    /// Requires every composite child to be a created, custom, simple policy. Two passes so
    /// `PolicyNotFound` (any non-existent child) takes precedence over `InvalidChildPolicy`
    /// (a built-in sentinel or a composite) across the whole set — the canonical revert order.
    fn validate_composite_child_policies<S: PolicyAccounting>(
        &self,
        storage: &S,
        child_policy_ids: &[u64],
    ) -> Result<()> {
        for &child in child_policy_ids {
            // Reverts PolicyNotFound when the child does not exist.
            self.require_existing_policy(storage, child)?;
        }
        for &child in child_policy_ids {
            if Self::is_builtin(child) || Self::is_composite(child) {
                return Err(BasePrecompileError::revert(IPolicyRegistry::InvalidChildPolicy {
                    childPolicyId: child,
                }));
            }
        }
        Ok(())
    }

    /// Evaluates a UNION (OR) composite over its live child set: authorized if any child
    /// authorizes. An empty set is unauthorized.
    fn is_authorized_union<S: PolicyAccounting>(
        &self,
        storage: &S,
        policy_id: u64,
        account: Address,
    ) -> Result<bool> {
        for child in storage.read_children(policy_id)? {
            if self.is_authorized(storage, child, account)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Evaluates an INTERSECT (AND) composite over its live child set: authorized only if every
    /// child authorizes. An empty set is authorized.
    fn is_authorized_intersect<S: PolicyAccounting>(
        &self,
        storage: &S,
        policy_id: u64,
        account: Address,
    ) -> Result<bool> {
        for child in storage.read_children(policy_id)? {
            if !self.is_authorized(storage, child, account)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Validates policy-creation inputs and returns the raw policy type discriminator.
    ///
    /// Only the simple leaf types are admissible here; `UNION`/`INTERSECT` must go through
    /// [`Self::create_composite_policy`], which is the only path that writes a child set. Inlined
    /// rather than shared with [`super::PolicyRegistryV1`] so that widening the rule for a later
    /// fork cannot reach back and change what that frozen version accepts.
    fn validate_create_policy_inputs(admin: Address, policy_type: PolicyType) -> Result<u8> {
        if admin == Address::ZERO {
            return Err(BasePrecompileError::revert(IPolicyRegistry::ZeroAddress {}));
        }
        if !matches!(policy_type, PolicyType::BLOCKLIST | PolicyType::ALLOWLIST) {
            return Err(BasePrecompileError::revert(IPolicyRegistry::IncompatiblePolicyType {}));
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
        let packed = self.require_existing_policy(storage, policy_id)?;
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

impl<S: PolicyAccounting> PolicyRegistryLogic<S> for PolicyRegistryV2 {
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

    fn create_composite_policy(
        &self,
        storage: &mut S,
        admin: Address,
        policy_type: PolicyType,
        child_policy_ids: Vec<u64>,
    ) -> Result<u64> {
        if admin == Address::ZERO {
            return Err(BasePrecompileError::revert(IPolicyRegistry::ZeroAddress {}));
        }
        if !matches!(policy_type, PolicyType::UNION | PolicyType::INTERSECT) {
            return Err(BasePrecompileError::revert(IPolicyRegistry::IncompatiblePolicyType {}));
        }
        Self::require_child_policy_in_range(&child_policy_ids)?;
        self.validate_composite_child_policies(storage, &child_policy_ids)?;

        // Reuse the simple create core for counter/ID/PolicyCreated+PolicyAdminUpdated events.
        let policy_id =
            self.create_policy_inner(storage, admin, policy_type, policy_type.as_discriminant())?;
        storage.write_children(policy_id, &child_policy_ids)?;
        storage.emit_event(
            IPolicyRegistry::CompositePolicyUpdated {
                policyId: policy_id,
                updater: storage.caller(),
                childPolicyIds: child_policy_ids,
            }
            .encode_log_data(),
        )?;
        Ok(policy_id)
    }

    fn update_composite(
        &self,
        storage: &mut S,
        policy_id: u64,
        child_policy_ids: Vec<u64>,
    ) -> Result<()> {
        let packed = self.require_existing_policy(storage, policy_id)?;
        if !Self::is_composite(policy_id) {
            return Err(BasePrecompileError::revert(IPolicyRegistry::IncompatiblePolicyType {}));
        }
        if packed.admin() != storage.caller() {
            return Err(BasePrecompileError::revert(IPolicyRegistry::Unauthorized {}));
        }
        Self::require_child_policy_in_range(&child_policy_ids)?;
        self.validate_composite_child_policies(storage, &child_policy_ids)?;

        storage.write_children(policy_id, &child_policy_ids)?;
        storage.emit_event(
            IPolicyRegistry::CompositePolicyUpdated {
                policyId: policy_id,
                updater: storage.caller(),
                childPolicyIds: child_policy_ids,
            }
            .encode_log_data(),
        )?;
        Ok(())
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
        let packed = self.require_existing_policy(storage, policy_id)?;
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
        // Built-in short-circuits precede any storage read: ALWAYS_ALLOW_ID = 0 is the EVM
        // default for any uninitialized policy field, so this must work before init has run.
        if policy_id == Self::ALWAYS_ALLOW_ID {
            return Ok(true);
        }
        if policy_id == Self::ALWAYS_BLOCK_ID {
            return Ok(false);
        }
        // Malformed IDs (type byte > INTERSECT) are treated as unauthorized rather than reverting.
        if !Self::is_well_formed(policy_id) {
            return Ok(false);
        }
        match Self::policy_id_type(policy_id) {
            Self::UNION_TYPE => self.is_authorized_union(storage, policy_id, account),
            Self::INTERSECT_TYPE => self.is_authorized_intersect(storage, policy_id, account),
            Self::ALLOWLIST_TYPE => storage.read_member(policy_id, account),
            Self::BLOCKLIST_TYPE => Ok(!storage.read_member(policy_id, account)?),
            _ => unreachable!("is_well_formed rejects type bytes > INTERSECT"),
        }
    }

    fn policy_exists(&self, storage: &S, policy_id: u64) -> Result<bool> {
        if policy_id == Self::ALWAYS_ALLOW_ID || policy_id == Self::ALWAYS_BLOCK_ID {
            return Ok(true);
        }
        // Malformed IDs (type byte > INTERSECT) are not well-formed, so they do not exist.
        if !Self::is_well_formed(policy_id) {
            return Ok(false);
        }
        Ok(PackedPolicy::from_raw(storage.read_policy_word(policy_id)?).exists())
    }

    fn get_policy_admin(&self, storage: &S, policy_id: u64) -> Result<Address> {
        if !Self::is_well_formed(policy_id) {
            return Ok(Address::ZERO);
        }
        let packed = PackedPolicy::from_raw(storage.read_policy_word(policy_id)?);
        if !packed.exists() {
            return Ok(Address::ZERO);
        }
        Ok(packed.admin())
    }

    fn pending_policy_admin(&self, storage: &S, policy_id: u64) -> Result<Address> {
        if !Self::is_well_formed(policy_id) {
            return Ok(Address::ZERO);
        }
        if policy_id == Self::ALWAYS_ALLOW_ID || policy_id == Self::ALWAYS_BLOCK_ID {
            return Ok(Address::ZERO);
        }
        storage.read_pending_admin(policy_id)
    }

    fn composite_policy_child_ids(&self, storage: &S, policy_id: u64) -> Result<Vec<u64>> {
        if !Self::is_well_formed(policy_id) {
            return Ok(Vec::new());
        }
        if !Self::is_composite(policy_id) {
            return Ok(Vec::new());
        }
        storage.read_children(policy_id)
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
        PolicyRegistryV2,
    };

    const REGISTRY: Address = address!("0x8453000000000000000000000000000000000002");
    const ADMIN: Address = address!("0x1000000000000000000000000000000000000001");
    const ALICE: Address = address!("0xA000000000000000000000000000000000000001");
    const BOB: Address = address!("0xB000000000000000000000000000000000000001");
    const NEW_ADMIN: Address = address!("0x2000000000000000000000000000000000000002");
    const LOGIC: PolicyRegistryV2 = PolicyRegistryV2;

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
        children: BTreeMap<u64, Vec<u64>>,
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
                children: BTreeMap::new(),
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
        fn read_children(&self, policy_id: u64) -> Result<Vec<u64>> {
            Ok(self.children.get(&policy_id).cloned().unwrap_or_default())
        }
        fn write_children(&mut self, policy_id: u64, child_policy_ids: &[u64]) -> Result<()> {
            self.children.insert(policy_id, child_policy_ids.to_vec());
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
        assert!(is_authorized(&rt, PolicyRegistryV2::ALWAYS_ALLOW_ID, ALICE));
        assert!(is_authorized(&rt, PolicyRegistryV2::ALWAYS_ALLOW_ID, BOB));
    }

    #[test]
    fn always_block_id_rejects_any_account() {
        let rt = initialized();
        assert!(!is_authorized(&rt, PolicyRegistryV2::ALWAYS_BLOCK_ID, ALICE));
        assert!(!is_authorized(&rt, PolicyRegistryV2::ALWAYS_BLOCK_ID, BOB));
    }

    #[test]
    fn unknown_blocklist_policy_id_authorizes_account() {
        // 0xdeadbeef has type byte 0 (BLOCKLIST); no members blocked => authorized.
        let rt = initialized();
        assert!(is_authorized(&rt, 0xdeadbeef, ALICE));
    }

    #[test]
    fn unknown_allowlist_policy_id_does_not_authorize_account() {
        let unknown_allowlist = PolicyRegistryV2::make_id(PolicyType::ALLOWLIST as u8, 9999);
        let rt = initialized();
        assert!(!is_authorized(&rt, unknown_allowlist, ALICE));
    }

    #[test]
    fn malformed_policy_id_is_authorized_returns_false() {
        // Type byte 4 is above INTERSECT (3), so it is malformed (not a composite).
        let malformed: u64 = (4u64 << 56) | 42;
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
        assert_eq!(id & PolicyRegistryV2::COUNTER_MASK, 2);
        assert!(LOGIC.policy_exists(&rt, PolicyRegistryV2::ALWAYS_ALLOW_ID).unwrap());
        assert!(LOGIC.policy_exists(&rt, PolicyRegistryV2::ALWAYS_BLOCK_ID).unwrap());
        assert!(rt.initialized);
    }

    #[test]
    fn ensure_initialized_and_get_counter_is_idempotent() {
        let mut rt = bare();
        for _ in 0..3 {
            LOGIC.ensure_initialized_and_get_counter(&mut rt).unwrap();
        }
        assert_eq!(rt.next_counter, PolicyRegistryV2::BUILTIN_POLICY_COUNT);
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
        assert_eq!(id1 & PolicyRegistryV2::COUNTER_MASK, 2);
        assert_eq!(id2 & PolicyRegistryV2::COUNTER_MASK, 3);
    }

    #[test]
    fn create_policy_at_counter_mask_reverts_with_under_overflow() {
        let mut rt = initialized();
        rt.next_counter = PolicyRegistryV2::COUNTER_MASK;
        let err = LOGIC.create_policy(&mut rt, ADMIN, PolicyType::ALLOWLIST).unwrap_err();
        assert_eq!(err, BasePrecompileError::under_overflow());
    }

    #[test]
    fn create_policy_at_counter_mask_minus_one_consumes_last_slot_then_reverts() {
        let mut rt = initialized();
        rt.next_counter = PolicyRegistryV2::COUNTER_MASK - 1;
        let id = LOGIC.create_policy(&mut rt, ADMIN, PolicyType::ALLOWLIST).unwrap();
        assert_eq!(id & PolicyRegistryV2::COUNTER_MASK, PolicyRegistryV2::COUNTER_MASK - 1);
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
        let accounts = many_accounts(PolicyRegistryV2::MAX_ACCOUNTS_PER_BATCH + 1);
        let err = LOGIC.update_allowlist(&mut rt, id, true, accounts).unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IPolicyRegistry::BatchSizeTooLarge {
                maxBatchSize: U256::from(PolicyRegistryV2::MAX_ACCOUNTS_PER_BATCH),
            })
        );
    }

    #[test]
    fn update_allowlist_max_batch_size_succeeds() {
        let mut rt = initialized();
        let id = create_allowlist(&mut rt);
        let accounts = many_accounts(PolicyRegistryV2::MAX_ACCOUNTS_PER_BATCH);
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
        let accounts = many_accounts(PolicyRegistryV2::MAX_ACCOUNTS_PER_BATCH + 1);
        let err = LOGIC.update_blocklist(&mut rt, id, true, accounts).unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IPolicyRegistry::BatchSizeTooLarge {
                maxBatchSize: U256::from(PolicyRegistryV2::MAX_ACCOUNTS_PER_BATCH),
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
        let accounts = many_accounts(PolicyRegistryV2::MAX_ACCOUNTS_PER_BATCH + 1);
        let err = LOGIC
            .create_policy_with_accounts(&mut rt, ADMIN, PolicyType::ALLOWLIST, accounts)
            .unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IPolicyRegistry::BatchSizeTooLarge {
                maxBatchSize: U256::from(PolicyRegistryV2::MAX_ACCOUNTS_PER_BATCH),
            })
        );
    }

    #[test]
    fn create_policy_with_accounts_zero_admin_precedes_batch_size_revert() {
        let mut rt = initialized();
        let accounts = many_accounts(PolicyRegistryV2::MAX_ACCOUNTS_PER_BATCH + 1);
        let err = LOGIC
            .create_policy_with_accounts(&mut rt, Address::ZERO, PolicyType::ALLOWLIST, accounts)
            .unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(IPolicyRegistry::ZeroAddress {}));
    }

    /// Precedence pin for the shared validator on the `createPolicyWithAccounts` path: a composite
    /// type is rejected with `IncompatiblePolicyType` before the batch-size guard runs, matching
    /// base-std's `ZeroAddress` -> `IncompatiblePolicyType` -> `BatchSizeTooLarge` order. Composite
    /// discriminants (2/3) are the only invalid-type values that decode at V2 and reach the logic;
    /// out-of-range bytes are rejected earlier at ABI decode (see `dispatch` tests).
    #[test]
    fn create_policy_with_accounts_composite_type_precedes_batch_size_revert() {
        let mut rt = initialized();
        let accounts = many_accounts(PolicyRegistryV2::MAX_ACCOUNTS_PER_BATCH + 1);
        for policy_type in [PolicyType::UNION, PolicyType::INTERSECT] {
            let err = LOGIC
                .create_policy_with_accounts(&mut rt, ADMIN, policy_type, accounts.clone())
                .unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IPolicyRegistry::IncompatiblePolicyType {})
            );
        }
    }

    /// `createPolicy` admits only simple leaf types. `create_composite_policy` is the sole path
    /// that validates children and writes a child set, so admitting a composite here would mint a
    /// gate whose ID says UNION/INTERSECT over an empty child list.
    #[test]
    fn create_policy_rejects_composite_types() {
        let mut rt = initialized();
        for policy_type in [PolicyType::UNION, PolicyType::INTERSECT] {
            let err = LOGIC.create_policy(&mut rt, ADMIN, policy_type).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IPolicyRegistry::IncompatiblePolicyType {})
            );
        }
    }

    /// Precedence pin: a zero admin is rejected before the composite-type check, matching the
    /// base-std natspec order (`ZeroAddress` before `IncompatiblePolicyType`).
    #[test]
    fn create_policy_zero_admin_precedes_incompatible_type() {
        let mut rt = initialized();
        for policy_type in [PolicyType::UNION, PolicyType::INTERSECT] {
            let err = LOGIC.create_policy(&mut rt, Address::ZERO, policy_type).unwrap_err();
            assert_eq!(err, BasePrecompileError::revert(IPolicyRegistry::ZeroAddress {}));
        }
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
        for policy_id in [PolicyRegistryV2::ALWAYS_ALLOW_ID, PolicyRegistryV2::ALWAYS_BLOCK_ID] {
            let err = LOGIC.stage_update_admin(&mut rt, policy_id, ALICE).unwrap_err();
            assert!(matches!(err, BasePrecompileError::Revert(_)));
        }
    }

    // --- read helpers for built-in / unknown / malformed IDs ---

    #[test]
    fn policy_exists_builtin_ids_always_return_true() {
        let rt = bare();
        assert!(LOGIC.policy_exists(&rt, PolicyRegistryV2::ALWAYS_ALLOW_ID).unwrap());
        assert!(LOGIC.policy_exists(&rt, PolicyRegistryV2::ALWAYS_BLOCK_ID).unwrap());
    }

    #[test]
    fn get_policy_admin_builtin_ids_return_zero_address() {
        let rt = initialized();
        assert_eq!(
            LOGIC.get_policy_admin(&rt, PolicyRegistryV2::ALWAYS_ALLOW_ID).unwrap(),
            Address::ZERO
        );
        assert_eq!(
            LOGIC.get_policy_admin(&rt, PolicyRegistryV2::ALWAYS_BLOCK_ID).unwrap(),
            Address::ZERO
        );
    }

    #[test]
    fn get_policy_admin_malformed_policy_id_returns_zero_address() {
        let rt = initialized();
        let malformed: u64 = (4u64 << 56) | 42;
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
            LOGIC.pending_policy_admin(&rt, PolicyRegistryV2::ALWAYS_ALLOW_ID).unwrap(),
            Address::ZERO
        );
        assert_eq!(
            LOGIC.pending_policy_admin(&rt, PolicyRegistryV2::ALWAYS_BLOCK_ID).unwrap(),
            Address::ZERO
        );
    }

    #[test]
    fn pending_policy_admin_builtin_ids_short_circuit_staged_slot() {
        let mut rt = initialized();
        for policy_id in [PolicyRegistryV2::ALWAYS_ALLOW_ID, PolicyRegistryV2::ALWAYS_BLOCK_ID] {
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
        let counter_one_blocklist = PolicyRegistryV2::make_id(PolicyType::BLOCKLIST as u8, 1);
        assert_ne!(counter_one_blocklist, PolicyRegistryV2::ALWAYS_BLOCK_ID);
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
        let malformed: u64 = (4u64 << 56) | 42;
        assert_eq!(LOGIC.pending_policy_admin(&rt, malformed).unwrap(), Address::ZERO);
    }

    #[test]
    fn composite_policy_child_ids_malformed_policy_id_returns_empty() {
        let rt = initialized();
        let malformed: u64 = (4u64 << 56) | 42;
        assert!(LOGIC.composite_policy_child_ids(&rt, malformed).unwrap().is_empty());
    }

    #[test]
    fn pending_policy_admin_nonexistent_well_formed_policy_returns_zero_address() {
        let rt = initialized();
        let nonexistent = PolicyRegistryV2::make_id(0, 999);
        assert_eq!(LOGIC.pending_policy_admin(&rt, nonexistent).unwrap(), Address::ZERO);
    }

    // --- composite policies (UNION / INTERSECT) ---

    fn create_union(rt: &mut Storage, children: Vec<u64>) -> u64 {
        set_caller(rt, ADMIN);
        LOGIC.create_composite_policy(rt, ADMIN, PolicyType::UNION, children).unwrap()
    }

    fn create_intersect(rt: &mut Storage, children: Vec<u64>) -> u64 {
        set_caller(rt, ADMIN);
        LOGIC.create_composite_policy(rt, ADMIN, PolicyType::INTERSECT, children).unwrap()
    }

    #[test]
    fn create_composite_encodes_gate_in_top_byte_and_is_observable() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let b = create_allowlist(&mut rt);
        let id = create_union(&mut rt, vec![a, b]);
        assert_eq!((id >> 56) as u8, PolicyType::UNION as u8);
        assert!(LOGIC.policy_exists(&rt, id).unwrap());
        assert_eq!(LOGIC.get_policy_admin(&rt, id).unwrap(), ADMIN);
    }

    #[test]
    fn create_composite_emits_created_admin_and_composite_updated_events() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let b = create_allowlist(&mut rt);
        let before = rt.events.len();
        let id = create_union(&mut rt, vec![a, b]);
        let events = &rt.events[before..];
        assert_eq!(events.len(), 3);
        assert_eq!(
            IPolicyRegistry::PolicyCreated::decode_log_data(&events[0]).unwrap().policyId,
            id
        );
        assert_eq!(
            IPolicyRegistry::PolicyAdminUpdated::decode_log_data(&events[1]).unwrap().newAdmin,
            ADMIN
        );
        let updated = IPolicyRegistry::CompositePolicyUpdated::decode_log_data(&events[2]).unwrap();
        assert_eq!(updated.policyId, id);
        assert_eq!(updated.updater, ADMIN);
        assert_eq!(updated.childPolicyIds, vec![a, b]);
    }

    #[test]
    fn union_is_authorized_if_any_child_authorizes() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let b = create_allowlist(&mut rt);
        LOGIC.update_allowlist(&mut rt, a, true, vec![ALICE]).unwrap();
        LOGIC.update_allowlist(&mut rt, b, true, vec![BOB]).unwrap();
        let id = create_union(&mut rt, vec![a, b]);
        assert!(LOGIC.is_authorized(&rt, id, ALICE).unwrap());
        assert!(LOGIC.is_authorized(&rt, id, BOB).unwrap());
        assert!(!LOGIC.is_authorized(&rt, id, NEW_ADMIN).unwrap());
    }

    #[test]
    fn intersect_is_authorized_only_if_all_children_authorize() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let b = create_allowlist(&mut rt);
        LOGIC.update_allowlist(&mut rt, a, true, vec![ALICE, BOB]).unwrap();
        LOGIC.update_allowlist(&mut rt, b, true, vec![ALICE]).unwrap();
        let id = create_intersect(&mut rt, vec![a, b]);
        assert!(LOGIC.is_authorized(&rt, id, ALICE).unwrap());
        assert!(!LOGIC.is_authorized(&rt, id, BOB).unwrap());
    }

    #[test]
    fn composite_evaluates_children_live() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let b = create_allowlist(&mut rt);
        LOGIC.update_allowlist(&mut rt, a, true, vec![ALICE]).unwrap();
        let id = create_intersect(&mut rt, vec![a, b]);
        // ALICE is in A but not B, so the intersection rejects.
        assert!(!LOGIC.is_authorized(&rt, id, ALICE).unwrap());
        // Adding ALICE to B flips the result live, without touching the composite.
        LOGIC.update_allowlist(&mut rt, b, true, vec![ALICE]).unwrap();
        assert!(LOGIC.is_authorized(&rt, id, ALICE).unwrap());
    }

    #[test]
    fn update_composite_replaces_child_set() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let b = create_allowlist(&mut rt);
        let c = create_allowlist(&mut rt);
        LOGIC.update_allowlist(&mut rt, a, true, vec![ALICE]).unwrap();
        LOGIC.update_allowlist(&mut rt, c, true, vec![BOB]).unwrap();
        let id = create_union(&mut rt, vec![a, b]);
        assert!(LOGIC.is_authorized(&rt, id, ALICE).unwrap());
        assert!(!LOGIC.is_authorized(&rt, id, BOB).unwrap());
        // Replace [a, b] with [b, c]: ALICE (only in a) drops out, BOB (in c) joins.
        set_caller(&mut rt, ADMIN);
        LOGIC.update_composite(&mut rt, id, vec![b, c]).unwrap();
        assert!(!LOGIC.is_authorized(&rt, id, ALICE).unwrap());
        assert!(LOGIC.is_authorized(&rt, id, BOB).unwrap());
    }

    #[test]
    fn composite_policy_child_ids_returns_the_set_in_creation_order() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let b = create_allowlist(&mut rt);
        let c = create_allowlist(&mut rt);
        let id = create_union(&mut rt, vec![c, a, b]);
        // Order is preserved verbatim; the registry never sorts or dedupes the child set.
        assert_eq!(LOGIC.composite_policy_child_ids(&rt, id).unwrap(), vec![c, a, b]);
    }

    #[test]
    fn composite_policy_child_ids_tracks_update_composite() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let b = create_allowlist(&mut rt);
        let c = create_allowlist(&mut rt);
        let id = create_intersect(&mut rt, vec![a, b]);
        assert_eq!(LOGIC.composite_policy_child_ids(&rt, id).unwrap(), vec![a, b]);
        set_caller(&mut rt, ADMIN);
        LOGIC.update_composite(&mut rt, id, vec![b, c]).unwrap();
        // Full replacement, not a merge — `a` is gone rather than retained as a stale tail.
        assert_eq!(LOGIC.composite_policy_child_ids(&rt, id).unwrap(), vec![b, c]);
    }

    #[test]
    fn composite_policy_child_ids_matches_the_emitted_event() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let b = create_allowlist(&mut rt);
        let before = rt.events.len();
        let id = create_union(&mut rt, vec![a, b]);
        let emitted =
            IPolicyRegistry::CompositePolicyUpdated::decode_log_data(&rt.events[before + 2])
                .unwrap()
                .childPolicyIds;
        assert_eq!(LOGIC.composite_policy_child_ids(&rt, id).unwrap(), emitted);
    }

    #[test]
    fn composite_policy_child_ids_returns_empty_for_non_composites() {
        let mut rt = initialized();
        let simple = create_allowlist(&mut rt);
        let malformed = PolicyRegistryV2::make_id(9, 1);
        let uncreated_union = PolicyRegistryV2::make_id(PolicyType::UNION as u8, 999);

        for policy_id in [
            simple,
            PolicyRegistryV2::ALWAYS_ALLOW_ID,
            PolicyRegistryV2::ALWAYS_BLOCK_ID,
            malformed,
            uncreated_union,
        ] {
            assert!(
                LOGIC.composite_policy_child_ids(&rt, policy_id).unwrap().is_empty(),
                "expected an empty child set for policy {policy_id}"
            );
        }
    }

    #[test]
    fn create_composite_zero_admin_reverts() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let b = create_allowlist(&mut rt);
        let err = LOGIC
            .create_composite_policy(&mut rt, Address::ZERO, PolicyType::UNION, vec![a, b])
            .unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(IPolicyRegistry::ZeroAddress {}));
    }

    #[test]
    fn create_composite_non_composite_type_reverts() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let b = create_allowlist(&mut rt);
        let err = LOGIC
            .create_composite_policy(&mut rt, ADMIN, PolicyType::ALLOWLIST, vec![a, b])
            .unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(IPolicyRegistry::IncompatiblePolicyType {}));
    }

    #[test]
    fn create_composite_child_count_out_of_range_reverts() {
        let mut rt = initialized();
        let ids: Vec<u64> = (0..5).map(|_| create_allowlist(&mut rt)).collect();
        let range_err =
            BasePrecompileError::revert(IPolicyRegistry::ChildPoliciesOutsideOfRange {});
        // Too few (1) and too many (5) both revert with the same range error.
        let too_few = LOGIC
            .create_composite_policy(&mut rt, ADMIN, PolicyType::UNION, vec![ids[0]])
            .unwrap_err();
        let too_many =
            LOGIC.create_composite_policy(&mut rt, ADMIN, PolicyType::UNION, ids).unwrap_err();
        assert_eq!(too_few, range_err);
        assert_eq!(too_many, range_err);
    }

    #[test]
    fn create_composite_nonexistent_child_reverts_policy_not_found() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let missing = PolicyRegistryV2::make_id(PolicyType::ALLOWLIST as u8, 999);
        let err = LOGIC
            .create_composite_policy(&mut rt, ADMIN, PolicyType::UNION, vec![a, missing])
            .unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(IPolicyRegistry::PolicyNotFound {}));
    }

    #[test]
    fn create_composite_builtin_child_reverts_invalid_child() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let err = LOGIC
            .create_composite_policy(
                &mut rt,
                ADMIN,
                PolicyType::UNION,
                vec![a, PolicyRegistryV2::ALWAYS_ALLOW_ID],
            )
            .unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IPolicyRegistry::InvalidChildPolicy {
                childPolicyId: PolicyRegistryV2::ALWAYS_ALLOW_ID,
            })
        );
    }

    #[test]
    fn create_composite_composite_child_reverts_invalid_child() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let b = create_allowlist(&mut rt);
        let inner = create_union(&mut rt, vec![a, b]);
        let c = create_allowlist(&mut rt);
        let err = LOGIC
            .create_composite_policy(&mut rt, ADMIN, PolicyType::INTERSECT, vec![c, inner])
            .unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IPolicyRegistry::InvalidChildPolicy {
                childPolicyId: inner
            })
        );
    }

    #[test]
    fn create_composite_nonexistent_child_precedes_invalid_child() {
        // Two-pass validation: a missing child reverts PolicyNotFound even when another child
        // is an invalid built-in (which would otherwise revert InvalidChildPolicy).
        let mut rt = initialized();
        let missing = PolicyRegistryV2::make_id(PolicyType::ALLOWLIST as u8, 999);
        let err = LOGIC
            .create_composite_policy(
                &mut rt,
                ADMIN,
                PolicyType::UNION,
                vec![PolicyRegistryV2::ALWAYS_ALLOW_ID, missing],
            )
            .unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(IPolicyRegistry::PolicyNotFound {}));
    }

    #[test]
    fn update_composite_nonexistent_reverts_policy_not_found() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let b = create_allowlist(&mut rt);
        let missing = PolicyRegistryV2::make_id(PolicyType::UNION as u8, 999);
        let err = LOGIC.update_composite(&mut rt, missing, vec![a, b]).unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(IPolicyRegistry::PolicyNotFound {}));
    }

    #[test]
    fn update_composite_on_simple_policy_reverts_incompatible_type() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let b = create_allowlist(&mut rt);
        let err = LOGIC.update_composite(&mut rt, a, vec![a, b]).unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(IPolicyRegistry::IncompatiblePolicyType {}));
    }

    #[test]
    fn update_composite_unauthorized_reverts() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let b = create_allowlist(&mut rt);
        let id = create_union(&mut rt, vec![a, b]);
        set_caller(&mut rt, ALICE);
        let err = LOGIC.update_composite(&mut rt, id, vec![a, b]).unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(IPolicyRegistry::Unauthorized {}));
    }

    #[test]
    fn update_composite_invalid_child_reverts() {
        let mut rt = initialized();
        let a = create_allowlist(&mut rt);
        let b = create_allowlist(&mut rt);
        let id = create_union(&mut rt, vec![a, b]);
        set_caller(&mut rt, ADMIN);
        let err = LOGIC
            .update_composite(&mut rt, id, vec![a, PolicyRegistryV2::ALWAYS_BLOCK_ID])
            .unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IPolicyRegistry::InvalidChildPolicy {
                childPolicyId: PolicyRegistryV2::ALWAYS_BLOCK_ID,
            })
        );
    }
}
