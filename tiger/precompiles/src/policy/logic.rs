//! Business logic of the `PolicyRegistry` precompile, for every version.
//!
//! One implementation serves all versions: V1 (Beryl) has allowlists and blocklists, V2
//! (Cobalt) adds UNION/INTERSECT composite policies, and V3 (Denim) adds inverted policy IDs.
//! Each difference is a check on [`PolicyRegistry::version`]; selectors a version does not
//! declare never reach here, since the dispatcher gates them on the version's ABI.

use alloc::vec::Vec;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolEvent;

use crate::{
    BasePrecompileError, IPolicyRegistry, IPolicyRegistry::PolicyType, PackedPolicy,
    PolicyAccounting, PolicyVersion, Result,
};

/// The `PolicyRegistry` logic of one version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyRegistry {
    /// The active version.
    pub version: PolicyVersion,
}

impl PolicyRegistry {
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

    /// (V3) Bit 63 of a policy ID. When set, authorization evaluates the base policy and negates
    /// the result without creating or storing a second policy record.
    pub const INVERTED_POLICY_BIT: u64 = 1u64 << 63;

    /// Maximum number of accounts per membership batch (`createPolicyWithAccounts`,
    /// `updateAllowlist`, `updateBlocklist`).
    pub const MAX_ACCOUNTS_PER_BATCH: usize = 64;

    /// (V2) Minimum number of child policies a composite must reference.
    pub const MIN_CHILD_POLICIES: usize = 2;

    /// (V2) Maximum number of child policies a composite may reference. Distinct from
    /// [`Self::MAX_ACCOUNTS_PER_BATCH`], which caps account-membership batches.
    pub const MAX_CHILD_POLICIES: usize = 4;

    const ALLOWLIST_TYPE: u8 = PolicyType::ALLOWLIST as u8;
    const BLOCKLIST_TYPE: u8 = PolicyType::BLOCKLIST as u8;
    const UNION_TYPE: u8 = PolicyType::UNION as u8;
    const INTERSECT_TYPE: u8 = PolicyType::INTERSECT as u8;
    const POLICY_ID_TYPE_SHIFT: usize = 56;

    /// The logic of `version`.
    pub const fn new(version: PolicyVersion) -> Self {
        Self { version }
    }

    /// Returns the policy type encoded in the top byte of `policy_id`.
    const fn policy_id_type(policy_id: u64) -> u8 {
        (policy_id >> Self::POLICY_ID_TYPE_SHIFT) as u8
    }

    /// Encodes a policy ID from its type discriminant and counter.
    pub const fn make_id(policy_type: u8, counter: u64) -> u64 {
        (policy_type as u64) << Self::POLICY_ID_TYPE_SHIFT | (counter & Self::COUNTER_MASK)
    }

    /// Returns `policy_id` with its invert flag toggled.
    pub const fn inverted_policy_id(policy_id: u64) -> u64 {
        policy_id ^ Self::INVERTED_POLICY_BIT
    }

    /// Whether inverted policy IDs exist (V3).
    fn inverts(self) -> bool {
        self.version >= PolicyVersion::V3
    }

    /// Removes the invert flag (from V3) so a read can resolve the stored base policy.
    fn base_policy_id(self, policy_id: u64) -> u64 {
        if self.inverts() { policy_id & !Self::INVERTED_POLICY_BIT } else { policy_id }
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

    /// Returns whether `policy_id`'s top byte is a type this version supports: simple types in
    /// V1, simple or composite from V2. Higher type bytes are malformed.
    fn is_well_formed(self, policy_id: u64) -> bool {
        let highest = if self.version >= PolicyVersion::V2 {
            Self::INTERSECT_TYPE
        } else {
            Self::ALLOWLIST_TYPE
        };
        Self::policy_id_type(policy_id) <= highest
    }

    /// Returns whether `policy_id` is a built-in sentinel (`ALWAYS_ALLOW` / `ALWAYS_BLOCK`).
    const fn is_builtin(policy_id: u64) -> bool {
        policy_id == Self::ALWAYS_ALLOW_ID || policy_id == Self::ALWAYS_BLOCK_ID
    }

    /// Requires a composite's children to number within `[2, 4]` and each to be a created,
    /// custom, simple policy. Two passes so `PolicyNotFound` (any non-existent child) takes
    /// precedence over `InvalidChildPolicy` (a built-in sentinel or a composite) across the whole
    /// set — the canonical revert order.
    fn validate_children<S: PolicyAccounting>(
        &self,
        storage: &S,
        child_policy_ids: &[u64],
    ) -> Result<()> {
        let count = child_policy_ids.len();
        if !(Self::MIN_CHILD_POLICIES..=Self::MAX_CHILD_POLICIES).contains(&count) {
            return Err(BasePrecompileError::revert(
                IPolicyRegistry::ChildPoliciesOutsideOfRange {},
            ));
        }
        for &child in child_policy_ids {
            // Reverts PolicyNotFound when the child's base policy does not exist.
            self.require_existing_policy(storage, self.base_policy_id(child))?;
        }
        for &child in child_policy_ids {
            let base = self.base_policy_id(child);
            if Self::is_builtin(base) || Self::is_composite(base) {
                return Err(BasePrecompileError::revert(IPolicyRegistry::InvalidChildPolicy {
                    childPolicyId: child,
                }));
            }
        }
        Ok(())
    }

    /// Writes the children of composite `policy_id` and emits `CompositePolicyUpdated`.
    fn write_children<S: PolicyAccounting>(
        storage: &mut S,
        policy_id: u64,
        child_policy_ids: Vec<u64>,
    ) -> Result<()> {
        storage.write_children(policy_id, &child_policy_ids)?;
        let updater = storage.caller();
        storage.emit_event(
            IPolicyRegistry::CompositePolicyUpdated {
                policyId: policy_id,
                updater,
                childPolicyIds: child_policy_ids,
            }
            .encode_log_data(),
        )
    }

    /// Emits `PolicyAdminUpdated`.
    fn emit_admin_updated<S: PolicyAccounting>(
        storage: &mut S,
        policy_id: u64,
        previous_admin: Address,
        new_admin: Address,
    ) -> Result<()> {
        storage.emit_event(
            IPolicyRegistry::PolicyAdminUpdated {
                policyId: policy_id,
                previousAdmin: previous_admin,
                newAdmin: new_admin,
            }
            .encode_log_data(),
        )
    }

    /// Validates policy-creation inputs and returns the raw policy type discriminator.
    ///
    /// Only the simple leaf types are admissible here; `UNION`/`INTERSECT` must go through
    /// [`Self::create_composite_policy`]. V1 checks the type first and fails it with an enum
    /// conversion error; from V2 the zero admin comes first and a composite type reverts
    /// `IncompatiblePolicyType`.
    fn validate_create_policy_inputs(self, admin: Address, policy_type: PolicyType) -> Result<u8> {
        let simple = matches!(policy_type, PolicyType::BLOCKLIST | PolicyType::ALLOWLIST);
        if self.version == PolicyVersion::V1 && !simple {
            return Err(BasePrecompileError::enum_conversion_error());
        }
        if admin == Address::ZERO {
            return Err(BasePrecompileError::revert(IPolicyRegistry::ZeroAddress {}));
        }
        if !simple {
            return Err(BasePrecompileError::revert(IPolicyRegistry::IncompatiblePolicyType {}));
        }
        Ok(policy_type as u8)
    }

    /// First-touch setup for the registry: writes the bytecode marker and the two built-in
    /// policies, then leaves the counter at [`Self::BUILTIN_POLICY_COUNT`].
    ///
    /// Gated on the counter, so subsequent calls cost a single read and bail. The bytecode
    /// marker must precede any storage write because the EVM path can prune writes made
    /// under an empty native-precompile account.
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
        const { assert!(Self::make_id(PolicyType::BLOCKLIST as u8, 0) == Self::ALWAYS_ALLOW_ID) };
        const { assert!(Self::make_id(PolicyType::ALLOWLIST as u8, 1) == Self::ALWAYS_BLOCK_ID) };
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
        if counter >= Self::COUNTER_MASK {
            return Err(BasePrecompileError::under_overflow());
        }
        storage.write_next_counter(counter + 1)?;
        let policy_id = Self::make_id(policy_type_u8, counter);
        storage.write_policy_word(policy_id, PackedPolicy::new(admin).into_u256())?;

        let creator = storage.caller();
        storage.emit_event(
            IPolicyRegistry::PolicyCreated { policyId: policy_id, creator, policyType: policy_type }
                .encode_log_data(),
        )?;
        let previous_admin = Address::ZERO;
        Self::emit_admin_updated(storage, policy_id, previous_admin, admin)?;

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

    /// Creates a new ALLOWLIST or BLOCKLIST policy, returning its encoded ID.
    pub fn create_policy<S: PolicyAccounting>(
        &self,
        storage: &mut S,
        admin: Address,
        policy_type: PolicyType,
    ) -> Result<u64> {
        let policy_type_u8 = self.validate_create_policy_inputs(admin, policy_type)?;
        self.create_policy_inner(storage, admin, policy_type, policy_type_u8)
    }

    /// Creates a new policy and seeds it with an initial member list.
    pub fn create_policy_with_accounts<S: PolicyAccounting>(
        &self,
        storage: &mut S,
        admin: Address,
        policy_type: PolicyType,
        accounts: Vec<Address>,
    ) -> Result<u64> {
        let policy_type_u8 = self.validate_create_policy_inputs(admin, policy_type)?;
        Self::require_account_batch_size(&accounts)?;
        let policy_id = self.create_policy_inner(storage, admin, policy_type, policy_type_u8)?;
        let updater = storage.caller();
        for account in &accounts {
            storage.set_member(policy_id, *account)?;
        }
        let (id, allowed, blocked) = (policy_id, true, true);
        let log = match policy_type {
            PolicyType::ALLOWLIST => {
                IPolicyRegistry::AllowlistUpdated { policyId: id, updater, allowed, accounts }
                    .encode_log_data()
            }
            PolicyType::BLOCKLIST => {
                IPolicyRegistry::BlocklistUpdated { policyId: id, updater, blocked, accounts }
                    .encode_log_data()
            }
            _ => return Err(BasePrecompileError::enum_conversion_error()),
        };
        storage.emit_event(log)?;
        Ok(policy_id)
    }

    /// (V2) Creates a composite (UNION/INTERSECT) policy over existing simple child policies.
    pub fn create_composite_policy<S: PolicyAccounting>(
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
        self.validate_children(storage, &child_policy_ids)?;
        // Reuse the simple create core for counter/ID/PolicyCreated+PolicyAdminUpdated events.
        let policy_id =
            self.create_policy_inner(storage, admin, policy_type, policy_type as u8)?;
        Self::write_children(storage, policy_id, child_policy_ids)?;
        Ok(policy_id)
    }

    /// (V2) Replaces a composite policy's child set in full.
    pub fn update_composite<S: PolicyAccounting>(
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
        self.validate_children(storage, &child_policy_ids)?;
        Self::write_children(storage, policy_id, child_policy_ids)
    }

    /// Stages a pending admin transfer for `policy_id`.
    ///
    /// Passing `Address::ZERO` clears a previously staged transfer without nominating a
    /// replacement.
    pub fn stage_update_admin<S: PolicyAccounting>(
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
        )
    }

    /// Completes a pending admin transfer; caller must be the staged pending admin.
    pub fn finalize_update_admin<S: PolicyAccounting>(
        &self,
        storage: &mut S,
        policy_id: u64,
    ) -> Result<()> {
        let packed = self.require_existing_policy(storage, policy_id)?;
        let pending = storage.read_pending_admin(policy_id)?;
        if pending == Address::ZERO {
            return Err(BasePrecompileError::revert(IPolicyRegistry::NoPendingAdmin {}));
        }
        let caller = storage.caller();
        if pending != caller {
            return Err(BasePrecompileError::revert(IPolicyRegistry::Unauthorized {}));
        }
        storage.write_policy_word(policy_id, PackedPolicy::new(caller).into_u256())?;
        storage.delete_pending_admin(policy_id)?;
        Self::emit_admin_updated(storage, policy_id, packed.admin(), caller)
    }

    /// Permanently relinquishes admin of `policy_id`.
    pub fn renounce_admin<S: PolicyAccounting>(&self, storage: &mut S, policy_id: u64) -> Result<()> {
        let (_, caller) = self.require_admin(storage, policy_id)?;
        storage.write_policy_word(policy_id, PackedPolicy::new(Address::ZERO).into_u256())?;
        storage.delete_pending_admin(policy_id)?;
        Self::emit_admin_updated(storage, policy_id, caller, Address::ZERO)
    }

    /// Adds or removes `accounts` from an ALLOWLIST policy's member set.
    pub fn update_allowlist<S: PolicyAccounting>(
        &self,
        storage: &mut S,
        policy_id: u64,
        allowed: bool,
        accounts: Vec<Address>,
    ) -> Result<()> {
        let updater =
            self.update_membership(storage, policy_id, Self::ALLOWLIST_TYPE, allowed, &accounts)?;
        let id = policy_id;
        let log = IPolicyRegistry::AllowlistUpdated { policyId: id, updater, allowed, accounts };
        storage.emit_event(log.encode_log_data())
    }

    /// Adds or removes `accounts` from a BLOCKLIST policy's member set.
    pub fn update_blocklist<S: PolicyAccounting>(
        &self,
        storage: &mut S,
        policy_id: u64,
        blocked: bool,
        accounts: Vec<Address>,
    ) -> Result<()> {
        let updater =
            self.update_membership(storage, policy_id, Self::BLOCKLIST_TYPE, blocked, &accounts)?;
        let id = policy_id;
        let log = IPolicyRegistry::BlocklistUpdated { policyId: id, updater, blocked, accounts };
        storage.emit_event(log.encode_log_data())
    }

    /// Returns whether `account` is authorized under `policy_id`.
    pub fn is_authorized<S: PolicyAccounting>(
        &self,
        storage: &S,
        policy_id: u64,
        account: Address,
    ) -> Result<bool> {
        // Built-in short-circuits precede any storage read: ALWAYS_ALLOW_ID = 0 is the EVM
        // default for any uninitialized policy field, so this must work before init has run.
        // Inverted built-ins have a different ID and therefore fall through to the invert branch.
        if policy_id == Self::ALWAYS_ALLOW_ID {
            return Ok(true);
        }
        if policy_id == Self::ALWAYS_BLOCK_ID {
            return Ok(false);
        }
        if self.inverts() && policy_id & Self::INVERTED_POLICY_BIT != 0 {
            let base = self.base_policy_id(policy_id);
            if !self.policy_exists(storage, base)? {
                return Ok(false);
            }
            return self.is_authorized(storage, base, account).map(|authorized| !authorized);
        }
        // Malformed IDs are treated as unauthorized rather than reverting.
        if !self.is_well_formed(policy_id) {
            return Ok(false);
        }
        match Self::policy_id_type(policy_id) {
            Self::UNION_TYPE => {
                for child in storage.read_children(policy_id)? {
                    if self.is_authorized(storage, child, account)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Self::INTERSECT_TYPE => {
                for child in storage.read_children(policy_id)? {
                    if !self.is_authorized(storage, child, account)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Self::ALLOWLIST_TYPE => storage.read_member(policy_id, account),
            Self::BLOCKLIST_TYPE => Ok(!storage.read_member(policy_id, account)?),
            _ => unreachable!("is_well_formed rejects type bytes > INTERSECT"),
        }
    }

    /// Returns whether `policy_id` refers to a built-in or previously created policy.
    pub fn policy_exists<S: PolicyAccounting>(&self, storage: &S, policy_id: u64) -> Result<bool> {
        let policy_id = self.base_policy_id(policy_id);
        if Self::is_builtin(policy_id) {
            return Ok(true);
        }
        if !self.is_well_formed(policy_id) {
            return Ok(false);
        }
        Ok(PackedPolicy::from_raw(storage.read_policy_word(policy_id)?).exists())
    }

    /// Returns the current admin of `policy_id`, or `Address::ZERO` if none / malformed.
    pub fn get_policy_admin<S: PolicyAccounting>(
        &self,
        storage: &S,
        policy_id: u64,
    ) -> Result<Address> {
        let policy_id = self.base_policy_id(policy_id);
        if !self.is_well_formed(policy_id) {
            return Ok(Address::ZERO);
        }
        let packed = PackedPolicy::from_raw(storage.read_policy_word(policy_id)?);
        if !packed.exists() {
            return Ok(Address::ZERO);
        }
        Ok(packed.admin())
    }

    /// Returns the staged pending admin for `policy_id`, or `Address::ZERO` if none.
    pub fn pending_policy_admin<S: PolicyAccounting>(
        &self,
        storage: &S,
        policy_id: u64,
    ) -> Result<Address> {
        let policy_id = self.base_policy_id(policy_id);
        if !self.is_well_formed(policy_id) || Self::is_builtin(policy_id) {
            return Ok(Address::ZERO);
        }
        storage.read_pending_admin(policy_id)
    }

    /// (V2) Returns a composite policy's child set, empty for anything else.
    pub fn composite_policy_child_ids<S: PolicyAccounting>(
        &self,
        storage: &S,
        policy_id: u64,
    ) -> Result<Vec<u64>> {
        let policy_id = self.base_policy_id(policy_id);
        if !self.is_well_formed(policy_id) || !Self::is_composite(policy_id) {
            return Ok(Vec::new());
        }
        storage.read_children(policy_id)
    }
}

#[cfg(test)]
mod tests {

    /// Version 1 (activated at Beryl).
    mod v1 {
        use alloc::{vec, vec::Vec};

        use alloy_primitives::{Address, U256, address};
        use alloy_sol_types::SolEvent;

        use crate::{
            BasePrecompileError, FakePolicyAccounting, IPolicyRegistry, IPolicyRegistry::PolicyType,
            PolicyRegistry, PolicyVersion, Result,
        };

        const ADMIN: Address = address!("0x1000000000000000000000000000000000000001");
        const ALICE: Address = address!("0xA000000000000000000000000000000000000001");
        const BOB: Address = address!("0xB000000000000000000000000000000000000001");
        const NEW_ADMIN: Address = address!("0x2000000000000000000000000000000000000002");
        const LOGIC: PolicyRegistry = PolicyRegistry::new(PolicyVersion::V1);

        type Storage = FakePolicyAccounting;

        /// Bare storage (no built-ins seeded), caller = `ADMIN`.
        fn bare() -> Storage {
            FakePolicyAccounting::with_caller(ADMIN)
        }

        /// Storage with both built-in policies seeded and the counter at 2.
        fn initialized() -> Storage {
            let mut storage = bare();
            LOGIC.ensure_initialized_and_get_counter(&mut storage).unwrap();
            storage
        }

        fn set_caller(storage: &mut Storage, caller: Address) {
            storage.set_caller(caller);
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
            assert!(is_authorized(&rt, PolicyRegistry::ALWAYS_ALLOW_ID, ALICE));
            assert!(is_authorized(&rt, PolicyRegistry::ALWAYS_ALLOW_ID, BOB));
        }

        #[test]
        fn always_block_id_rejects_any_account() {
            let rt = initialized();
            assert!(!is_authorized(&rt, PolicyRegistry::ALWAYS_BLOCK_ID, ALICE));
            assert!(!is_authorized(&rt, PolicyRegistry::ALWAYS_BLOCK_ID, BOB));
        }

        #[test]
        fn unknown_blocklist_policy_id_authorizes_account() {
            // 0xdeadbeef has type byte 0 (BLOCKLIST); no members blocked => authorized.
            let rt = initialized();
            assert!(is_authorized(&rt, 0xdeadbeef, ALICE));
        }

        #[test]
        fn unknown_allowlist_policy_id_does_not_authorize_account() {
            let unknown_allowlist = PolicyRegistry::make_id(PolicyType::ALLOWLIST as u8, 9999);
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
            assert_eq!(id & PolicyRegistry::COUNTER_MASK, 2);
            assert!(LOGIC.policy_exists(&rt, PolicyRegistry::ALWAYS_ALLOW_ID).unwrap());
            assert!(LOGIC.policy_exists(&rt, PolicyRegistry::ALWAYS_BLOCK_ID).unwrap());
            assert!(rt.initialized);
        }

        #[test]
        fn ensure_initialized_and_get_counter_is_idempotent() {
            let mut rt = bare();
            for _ in 0..3 {
                LOGIC.ensure_initialized_and_get_counter(&mut rt).unwrap();
            }
            assert_eq!(rt.next_counter, PolicyRegistry::BUILTIN_POLICY_COUNT);
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
            assert_eq!(id1 & PolicyRegistry::COUNTER_MASK, 2);
            assert_eq!(id2 & PolicyRegistry::COUNTER_MASK, 3);
        }

        #[test]
        fn create_policy_at_counter_mask_reverts_with_under_overflow() {
            let mut rt = initialized();
            rt.next_counter = PolicyRegistry::COUNTER_MASK;
            let err = LOGIC.create_policy(&mut rt, ADMIN, PolicyType::ALLOWLIST).unwrap_err();
            assert_eq!(err, BasePrecompileError::under_overflow());
        }

        #[test]
        fn create_policy_at_counter_mask_minus_one_consumes_last_slot_then_reverts() {
            let mut rt = initialized();
            rt.next_counter = PolicyRegistry::COUNTER_MASK - 1;
            let id = LOGIC.create_policy(&mut rt, ADMIN, PolicyType::ALLOWLIST).unwrap();
            assert_eq!(id & PolicyRegistry::COUNTER_MASK, PolicyRegistry::COUNTER_MASK - 1);
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
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH + 1);
            let err = LOGIC.update_allowlist(&mut rt, id, true, accounts).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IPolicyRegistry::BatchSizeTooLarge {
                    maxBatchSize: U256::from(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH),
                })
            );
        }

        #[test]
        fn update_allowlist_max_batch_size_succeeds() {
            let mut rt = initialized();
            let id = create_allowlist(&mut rt);
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH);
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
        fn update_blocklist_too_many_accounts_reverts() {
            let mut rt = initialized();
            let id = create_blocklist(&mut rt);
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH + 1);
            let err = LOGIC.update_blocklist(&mut rt, id, true, accounts).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IPolicyRegistry::BatchSizeTooLarge {
                    maxBatchSize: U256::from(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH),
                })
            );
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
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH + 1);
            let err = LOGIC
                .create_policy_with_accounts(&mut rt, ADMIN, PolicyType::ALLOWLIST, accounts)
                .unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IPolicyRegistry::BatchSizeTooLarge {
                    maxBatchSize: U256::from(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH),
                })
            );
        }

        #[test]
        fn create_policy_with_accounts_zero_admin_precedes_batch_size_revert() {
            let mut rt = initialized();
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH + 1);
            let err = LOGIC
                .create_policy_with_accounts(&mut rt, Address::ZERO, PolicyType::ALLOWLIST, accounts)
                .unwrap_err();
            assert_eq!(err, BasePrecompileError::revert(IPolicyRegistry::ZeroAddress {}));
        }

        #[test]
        fn create_policy_with_accounts_invalid_policy_type_precedes_batch_size_revert() {
            let mut rt = initialized();
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH + 1);
            let err = LOGIC
                .create_policy_with_accounts(&mut rt, ADMIN, PolicyType::__Invalid, accounts)
                .unwrap_err();
            assert_eq!(err, BasePrecompileError::enum_conversion_error());
        }

        /// Composite gates were not dialable at Beryl — this version's wire surface rejects their
        /// discriminants at decode. This pins the logic-level guard sitting behind that: `PolicyType`
        /// is the canonical (widened) enum, so a composite is representable here even though no
        /// calldata can produce one, and the frozen answer must stay `enum_conversion_error`.
        #[test]
        fn create_policy_rejects_composite_types() {
            let mut rt = initialized();
            for policy_type in [PolicyType::UNION, PolicyType::INTERSECT] {
                let err = LOGIC.create_policy(&mut rt, ADMIN, policy_type).unwrap_err();
                assert_eq!(err, BasePrecompileError::enum_conversion_error());
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
        fn finalize_update_admin_unauthorized_reverts() {
            let mut rt = initialized();
            let id = create_allowlist(&mut rt);
            LOGIC.stage_update_admin(&mut rt, id, NEW_ADMIN).unwrap();
            set_caller(&mut rt, ALICE);
            let err = LOGIC.finalize_update_admin(&mut rt, id).unwrap_err();
            assert!(matches!(err, BasePrecompileError::Revert(_)));
        }

        #[test]
        fn builtin_policies_reject_admin_mutations() {
            let mut rt = initialized();
            for policy_id in [PolicyRegistry::ALWAYS_ALLOW_ID, PolicyRegistry::ALWAYS_BLOCK_ID] {
                let err = LOGIC.stage_update_admin(&mut rt, policy_id, ALICE).unwrap_err();
                assert!(matches!(err, BasePrecompileError::Revert(_)));
            }
        }

        // --- read helpers for built-in / unknown / malformed IDs ---

        #[test]
        fn policy_exists_builtin_ids_always_return_true() {
            let rt = bare();
            assert!(LOGIC.policy_exists(&rt, PolicyRegistry::ALWAYS_ALLOW_ID).unwrap());
            assert!(LOGIC.policy_exists(&rt, PolicyRegistry::ALWAYS_BLOCK_ID).unwrap());
        }

        #[test]
        fn get_policy_admin_builtin_ids_return_zero_address() {
            let rt = initialized();
            assert_eq!(
                LOGIC.get_policy_admin(&rt, PolicyRegistry::ALWAYS_ALLOW_ID).unwrap(),
                Address::ZERO
            );
            assert_eq!(
                LOGIC.get_policy_admin(&rt, PolicyRegistry::ALWAYS_BLOCK_ID).unwrap(),
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
                LOGIC.pending_policy_admin(&rt, PolicyRegistry::ALWAYS_ALLOW_ID).unwrap(),
                Address::ZERO
            );
            assert_eq!(
                LOGIC.pending_policy_admin(&rt, PolicyRegistry::ALWAYS_BLOCK_ID).unwrap(),
                Address::ZERO
            );
        }

        #[test]
        fn pending_policy_admin_builtin_ids_short_circuit_staged_slot() {
            let mut rt = initialized();
            for policy_id in [PolicyRegistry::ALWAYS_ALLOW_ID, PolicyRegistry::ALWAYS_BLOCK_ID] {
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
            let counter_one_blocklist = PolicyRegistry::make_id(PolicyType::BLOCKLIST as u8, 1);
            assert_ne!(counter_one_blocklist, PolicyRegistry::ALWAYS_BLOCK_ID);
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
            let nonexistent = PolicyRegistry::make_id(0, 999);
            assert_eq!(LOGIC.pending_policy_admin(&rt, nonexistent).unwrap(), Address::ZERO);
        }

        fn rejected_operation_reverts(
            create: fn(&mut Storage) -> u64,
            caller: Address,
            operation: impl FnOnce(Storage, u64) -> Result<()>,
            expected: BasePrecompileError,
        ) {
            let mut rt = initialized();
            let id = create(&mut rt);
            set_caller(&mut rt, caller);
            assert_eq!(operation(rt, id).unwrap_err(), expected);
        }

        #[test]
        fn update_allowlist_on_blocklist_reverts() {
            rejected_operation_reverts(
                create_blocklist,
                ADMIN,
                |mut rt, id| LOGIC.update_allowlist(&mut rt, id, true, vec![ALICE]),
                BasePrecompileError::revert(IPolicyRegistry::IncompatiblePolicyType {}),
            );
        }

        #[test]
        fn update_blocklist_on_allowlist_reverts() {
            rejected_operation_reverts(
                create_allowlist,
                ADMIN,
                |mut rt, id| LOGIC.update_blocklist(&mut rt, id, true, vec![ALICE]),
                BasePrecompileError::revert(IPolicyRegistry::IncompatiblePolicyType {}),
            );
        }

        #[test]
        fn update_allowlist_on_blocklist_as_non_admin_reverts() {
            rejected_operation_reverts(
                create_blocklist,
                ALICE,
                |mut rt, id| LOGIC.update_allowlist(&mut rt, id, true, vec![ALICE]),
                BasePrecompileError::revert(IPolicyRegistry::IncompatiblePolicyType {}),
            );
        }

        #[test]
        fn update_blocklist_on_allowlist_as_non_admin_reverts() {
            rejected_operation_reverts(
                create_allowlist,
                ALICE,
                |mut rt, id| LOGIC.update_blocklist(&mut rt, id, true, vec![ALICE]),
                BasePrecompileError::revert(IPolicyRegistry::IncompatiblePolicyType {}),
            );
        }

        #[test]
        fn finalize_admin_without_pending_reverts() {
            rejected_operation_reverts(
                create_allowlist,
                ADMIN,
                |mut rt, id| LOGIC.finalize_update_admin(&mut rt, id),
                BasePrecompileError::revert(IPolicyRegistry::NoPendingAdmin {}),
            );
        }

        #[test]
        fn stage_admin_as_non_admin_reverts() {
            rejected_operation_reverts(
                create_allowlist,
                ALICE,
                |mut rt, id| LOGIC.stage_update_admin(&mut rt, id, NEW_ADMIN),
                BasePrecompileError::revert(IPolicyRegistry::Unauthorized {}),
            );
        }

        #[test]
        fn update_allowlist_after_renounce_reverts() {
            rejected_operation_reverts(
                create_allowlist,
                ADMIN,
                |mut rt, id| {
                    LOGIC.renounce_admin(&mut rt, id).unwrap();
                    LOGIC.update_allowlist(&mut rt, id, true, vec![ALICE])
                },
                BasePrecompileError::revert(IPolicyRegistry::Unauthorized {}),
            );
        }

        #[test]
        fn renounce_admin_as_non_admin_reverts() {
            rejected_operation_reverts(
                create_allowlist,
                ALICE,
                |mut rt, id| LOGIC.renounce_admin(&mut rt, id),
                BasePrecompileError::revert(IPolicyRegistry::Unauthorized {}),
            );
        }

    }

    /// Version 2 (activated at Cobalt).
    mod v2 {
        use alloc::{vec, vec::Vec};

        use alloy_primitives::{Address, U256, address};
        use alloy_sol_types::SolEvent;

        use crate::{
            BasePrecompileError, FakePolicyAccounting, IPolicyRegistry, IPolicyRegistry::PolicyType,
            PolicyRegistry, PolicyVersion,
        };

        const ADMIN: Address = address!("0x1000000000000000000000000000000000000001");
        const ALICE: Address = address!("0xA000000000000000000000000000000000000001");
        const BOB: Address = address!("0xB000000000000000000000000000000000000001");
        const NEW_ADMIN: Address = address!("0x2000000000000000000000000000000000000002");
        const LOGIC: PolicyRegistry = PolicyRegistry::new(PolicyVersion::V2);

        type Storage = FakePolicyAccounting;

        /// Bare storage (no built-ins seeded), caller = `ADMIN`.
        fn bare() -> Storage {
            FakePolicyAccounting::with_caller(ADMIN)
        }

        /// Storage with both built-in policies seeded and the counter at 2.
        fn initialized() -> Storage {
            let mut storage = bare();
            LOGIC.ensure_initialized_and_get_counter(&mut storage).unwrap();
            storage
        }

        fn set_caller(storage: &mut Storage, caller: Address) {
            storage.set_caller(caller);
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
            assert!(is_authorized(&rt, PolicyRegistry::ALWAYS_ALLOW_ID, ALICE));
            assert!(is_authorized(&rt, PolicyRegistry::ALWAYS_ALLOW_ID, BOB));
        }

        #[test]
        fn always_block_id_rejects_any_account() {
            let rt = initialized();
            assert!(!is_authorized(&rt, PolicyRegistry::ALWAYS_BLOCK_ID, ALICE));
            assert!(!is_authorized(&rt, PolicyRegistry::ALWAYS_BLOCK_ID, BOB));
        }

        #[test]
        fn unknown_blocklist_policy_id_authorizes_account() {
            // 0xdeadbeef has type byte 0 (BLOCKLIST); no members blocked => authorized.
            let rt = initialized();
            assert!(is_authorized(&rt, 0xdeadbeef, ALICE));
        }

        #[test]
        fn unknown_allowlist_policy_id_does_not_authorize_account() {
            let unknown_allowlist = PolicyRegistry::make_id(PolicyType::ALLOWLIST as u8, 9999);
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
            assert_eq!(id & PolicyRegistry::COUNTER_MASK, 2);
            assert!(LOGIC.policy_exists(&rt, PolicyRegistry::ALWAYS_ALLOW_ID).unwrap());
            assert!(LOGIC.policy_exists(&rt, PolicyRegistry::ALWAYS_BLOCK_ID).unwrap());
            assert!(rt.initialized);
        }

        #[test]
        fn ensure_initialized_and_get_counter_is_idempotent() {
            let mut rt = bare();
            for _ in 0..3 {
                LOGIC.ensure_initialized_and_get_counter(&mut rt).unwrap();
            }
            assert_eq!(rt.next_counter, PolicyRegistry::BUILTIN_POLICY_COUNT);
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
            assert_eq!(id1 & PolicyRegistry::COUNTER_MASK, 2);
            assert_eq!(id2 & PolicyRegistry::COUNTER_MASK, 3);
        }

        #[test]
        fn create_policy_at_counter_mask_reverts_with_under_overflow() {
            let mut rt = initialized();
            rt.next_counter = PolicyRegistry::COUNTER_MASK;
            let err = LOGIC.create_policy(&mut rt, ADMIN, PolicyType::ALLOWLIST).unwrap_err();
            assert_eq!(err, BasePrecompileError::under_overflow());
        }

        #[test]
        fn create_policy_at_counter_mask_minus_one_consumes_last_slot_then_reverts() {
            let mut rt = initialized();
            rt.next_counter = PolicyRegistry::COUNTER_MASK - 1;
            let id = LOGIC.create_policy(&mut rt, ADMIN, PolicyType::ALLOWLIST).unwrap();
            assert_eq!(id & PolicyRegistry::COUNTER_MASK, PolicyRegistry::COUNTER_MASK - 1);
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
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH + 1);
            let err = LOGIC.update_allowlist(&mut rt, id, true, accounts).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IPolicyRegistry::BatchSizeTooLarge {
                    maxBatchSize: U256::from(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH),
                })
            );
        }

        #[test]
        fn update_allowlist_max_batch_size_succeeds() {
            let mut rt = initialized();
            let id = create_allowlist(&mut rt);
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH);
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
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH + 1);
            let err = LOGIC.update_blocklist(&mut rt, id, true, accounts).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IPolicyRegistry::BatchSizeTooLarge {
                    maxBatchSize: U256::from(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH),
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
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH + 1);
            let err = LOGIC
                .create_policy_with_accounts(&mut rt, ADMIN, PolicyType::ALLOWLIST, accounts)
                .unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IPolicyRegistry::BatchSizeTooLarge {
                    maxBatchSize: U256::from(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH),
                })
            );
        }

        #[test]
        fn create_policy_with_accounts_zero_admin_precedes_batch_size_revert() {
            let mut rt = initialized();
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH + 1);
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
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH + 1);
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
            for policy_id in [PolicyRegistry::ALWAYS_ALLOW_ID, PolicyRegistry::ALWAYS_BLOCK_ID] {
                let err = LOGIC.stage_update_admin(&mut rt, policy_id, ALICE).unwrap_err();
                assert!(matches!(err, BasePrecompileError::Revert(_)));
            }
        }

        // --- read helpers for built-in / unknown / malformed IDs ---

        #[test]
        fn policy_exists_builtin_ids_always_return_true() {
            let rt = bare();
            assert!(LOGIC.policy_exists(&rt, PolicyRegistry::ALWAYS_ALLOW_ID).unwrap());
            assert!(LOGIC.policy_exists(&rt, PolicyRegistry::ALWAYS_BLOCK_ID).unwrap());
        }

        #[test]
        fn get_policy_admin_builtin_ids_return_zero_address() {
            let rt = initialized();
            assert_eq!(
                LOGIC.get_policy_admin(&rt, PolicyRegistry::ALWAYS_ALLOW_ID).unwrap(),
                Address::ZERO
            );
            assert_eq!(
                LOGIC.get_policy_admin(&rt, PolicyRegistry::ALWAYS_BLOCK_ID).unwrap(),
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
                LOGIC.pending_policy_admin(&rt, PolicyRegistry::ALWAYS_ALLOW_ID).unwrap(),
                Address::ZERO
            );
            assert_eq!(
                LOGIC.pending_policy_admin(&rt, PolicyRegistry::ALWAYS_BLOCK_ID).unwrap(),
                Address::ZERO
            );
        }

        #[test]
        fn pending_policy_admin_builtin_ids_short_circuit_staged_slot() {
            let mut rt = initialized();
            for policy_id in [PolicyRegistry::ALWAYS_ALLOW_ID, PolicyRegistry::ALWAYS_BLOCK_ID] {
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
            let counter_one_blocklist = PolicyRegistry::make_id(PolicyType::BLOCKLIST as u8, 1);
            assert_ne!(counter_one_blocklist, PolicyRegistry::ALWAYS_BLOCK_ID);
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
            let nonexistent = PolicyRegistry::make_id(0, 999);
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
            let malformed = PolicyRegistry::make_id(9, 1);
            let uncreated_union = PolicyRegistry::make_id(PolicyType::UNION as u8, 999);

            for policy_id in [
                simple,
                PolicyRegistry::ALWAYS_ALLOW_ID,
                PolicyRegistry::ALWAYS_BLOCK_ID,
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
            let missing = PolicyRegistry::make_id(PolicyType::ALLOWLIST as u8, 999);
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
                    vec![a, PolicyRegistry::ALWAYS_ALLOW_ID],
                )
                .unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IPolicyRegistry::InvalidChildPolicy {
                    childPolicyId: PolicyRegistry::ALWAYS_ALLOW_ID,
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
            let missing = PolicyRegistry::make_id(PolicyType::ALLOWLIST as u8, 999);
            let err = LOGIC
                .create_composite_policy(
                    &mut rt,
                    ADMIN,
                    PolicyType::UNION,
                    vec![PolicyRegistry::ALWAYS_ALLOW_ID, missing],
                )
                .unwrap_err();
            assert_eq!(err, BasePrecompileError::revert(IPolicyRegistry::PolicyNotFound {}));
        }

        #[test]
        fn update_composite_nonexistent_reverts_policy_not_found() {
            let mut rt = initialized();
            let a = create_allowlist(&mut rt);
            let b = create_allowlist(&mut rt);
            let missing = PolicyRegistry::make_id(PolicyType::UNION as u8, 999);
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
                .update_composite(&mut rt, id, vec![a, PolicyRegistry::ALWAYS_BLOCK_ID])
                .unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IPolicyRegistry::InvalidChildPolicy {
                    childPolicyId: PolicyRegistry::ALWAYS_BLOCK_ID,
                })
            );
        }

    }

    /// Version 3 (activated at Denim).
    mod v3 {
        use alloc::{vec, vec::Vec};

        use alloy_primitives::{Address, U256, address};
        use alloy_sol_types::SolEvent;

        use crate::{
            BasePrecompileError, FakePolicyAccounting, IPolicyRegistry, IPolicyRegistry::PolicyType,
            PolicyRegistry, PolicyVersion,
        };

        const ADMIN: Address = address!("0x1000000000000000000000000000000000000001");
        const ALICE: Address = address!("0xA000000000000000000000000000000000000001");
        const BOB: Address = address!("0xB000000000000000000000000000000000000001");
        const NEW_ADMIN: Address = address!("0x2000000000000000000000000000000000000002");
        const LOGIC: PolicyRegistry = PolicyRegistry::new(PolicyVersion::V3);

        type Storage = FakePolicyAccounting;

        /// Bare storage (no built-ins seeded), caller = `ADMIN`.
        fn bare() -> Storage {
            FakePolicyAccounting::with_caller(ADMIN)
        }

        /// Storage with both built-in policies seeded and the counter at 2.
        fn initialized() -> Storage {
            let mut storage = bare();
            LOGIC.ensure_initialized_and_get_counter(&mut storage).unwrap();
            storage
        }

        fn set_caller(storage: &mut Storage, caller: Address) {
            storage.set_caller(caller);
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
            assert!(is_authorized(&rt, PolicyRegistry::ALWAYS_ALLOW_ID, ALICE));
            assert!(is_authorized(&rt, PolicyRegistry::ALWAYS_ALLOW_ID, BOB));
        }

        #[test]
        fn always_block_id_rejects_any_account() {
            let rt = initialized();
            assert!(!is_authorized(&rt, PolicyRegistry::ALWAYS_BLOCK_ID, ALICE));
            assert!(!is_authorized(&rt, PolicyRegistry::ALWAYS_BLOCK_ID, BOB));
        }

        #[test]
        fn unknown_blocklist_policy_id_authorizes_account() {
            // 0xdeadbeef has type byte 0 (BLOCKLIST); no members blocked => authorized.
            let rt = initialized();
            assert!(is_authorized(&rt, 0xdeadbeef, ALICE));
        }

        #[test]
        fn unknown_allowlist_policy_id_does_not_authorize_account() {
            let unknown_allowlist = PolicyRegistry::make_id(PolicyType::ALLOWLIST as u8, 9999);
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
            assert_eq!(id & PolicyRegistry::COUNTER_MASK, 2);
            assert!(LOGIC.policy_exists(&rt, PolicyRegistry::ALWAYS_ALLOW_ID).unwrap());
            assert!(LOGIC.policy_exists(&rt, PolicyRegistry::ALWAYS_BLOCK_ID).unwrap());
            assert!(rt.initialized);
        }

        #[test]
        fn ensure_initialized_and_get_counter_is_idempotent() {
            let mut rt = bare();
            for _ in 0..3 {
                LOGIC.ensure_initialized_and_get_counter(&mut rt).unwrap();
            }
            assert_eq!(rt.next_counter, PolicyRegistry::BUILTIN_POLICY_COUNT);
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
            assert_eq!(id1 & PolicyRegistry::COUNTER_MASK, 2);
            assert_eq!(id2 & PolicyRegistry::COUNTER_MASK, 3);
        }

        #[test]
        fn create_policy_at_counter_mask_reverts_with_under_overflow() {
            let mut rt = initialized();
            rt.next_counter = PolicyRegistry::COUNTER_MASK;
            let err = LOGIC.create_policy(&mut rt, ADMIN, PolicyType::ALLOWLIST).unwrap_err();
            assert_eq!(err, BasePrecompileError::under_overflow());
        }

        #[test]
        fn create_policy_at_counter_mask_minus_one_consumes_last_slot_then_reverts() {
            let mut rt = initialized();
            rt.next_counter = PolicyRegistry::COUNTER_MASK - 1;
            let id = LOGIC.create_policy(&mut rt, ADMIN, PolicyType::ALLOWLIST).unwrap();
            assert_eq!(id & PolicyRegistry::COUNTER_MASK, PolicyRegistry::COUNTER_MASK - 1);
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
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH + 1);
            let err = LOGIC.update_allowlist(&mut rt, id, true, accounts).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IPolicyRegistry::BatchSizeTooLarge {
                    maxBatchSize: U256::from(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH),
                })
            );
        }

        #[test]
        fn update_allowlist_max_batch_size_succeeds() {
            let mut rt = initialized();
            let id = create_allowlist(&mut rt);
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH);
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
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH + 1);
            let err = LOGIC.update_blocklist(&mut rt, id, true, accounts).unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IPolicyRegistry::BatchSizeTooLarge {
                    maxBatchSize: U256::from(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH),
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
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH + 1);
            let err = LOGIC
                .create_policy_with_accounts(&mut rt, ADMIN, PolicyType::ALLOWLIST, accounts)
                .unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IPolicyRegistry::BatchSizeTooLarge {
                    maxBatchSize: U256::from(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH),
                })
            );
        }

        #[test]
        fn create_policy_with_accounts_zero_admin_precedes_batch_size_revert() {
            let mut rt = initialized();
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH + 1);
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
            let accounts = many_accounts(PolicyRegistry::MAX_ACCOUNTS_PER_BATCH + 1);
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
            for policy_id in [PolicyRegistry::ALWAYS_ALLOW_ID, PolicyRegistry::ALWAYS_BLOCK_ID] {
                let err = LOGIC.stage_update_admin(&mut rt, policy_id, ALICE).unwrap_err();
                assert!(matches!(err, BasePrecompileError::Revert(_)));
            }
        }

        // --- read helpers for built-in / unknown / malformed IDs ---

        #[test]
        fn policy_exists_builtin_ids_always_return_true() {
            let rt = bare();
            assert!(LOGIC.policy_exists(&rt, PolicyRegistry::ALWAYS_ALLOW_ID).unwrap());
            assert!(LOGIC.policy_exists(&rt, PolicyRegistry::ALWAYS_BLOCK_ID).unwrap());
        }

        #[test]
        fn get_policy_admin_builtin_ids_return_zero_address() {
            let rt = initialized();
            assert_eq!(
                LOGIC.get_policy_admin(&rt, PolicyRegistry::ALWAYS_ALLOW_ID).unwrap(),
                Address::ZERO
            );
            assert_eq!(
                LOGIC.get_policy_admin(&rt, PolicyRegistry::ALWAYS_BLOCK_ID).unwrap(),
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
                LOGIC.pending_policy_admin(&rt, PolicyRegistry::ALWAYS_ALLOW_ID).unwrap(),
                Address::ZERO
            );
            assert_eq!(
                LOGIC.pending_policy_admin(&rt, PolicyRegistry::ALWAYS_BLOCK_ID).unwrap(),
                Address::ZERO
            );
        }

        #[test]
        fn pending_policy_admin_builtin_ids_short_circuit_staged_slot() {
            let mut rt = initialized();
            for policy_id in [PolicyRegistry::ALWAYS_ALLOW_ID, PolicyRegistry::ALWAYS_BLOCK_ID] {
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
            let counter_one_blocklist = PolicyRegistry::make_id(PolicyType::BLOCKLIST as u8, 1);
            assert_ne!(counter_one_blocklist, PolicyRegistry::ALWAYS_BLOCK_ID);
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
            let nonexistent = PolicyRegistry::make_id(0, 999);
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
            let malformed = PolicyRegistry::make_id(9, 1);
            let uncreated_union = PolicyRegistry::make_id(PolicyType::UNION as u8, 999);

            for policy_id in [
                simple,
                PolicyRegistry::ALWAYS_ALLOW_ID,
                PolicyRegistry::ALWAYS_BLOCK_ID,
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
            let missing = PolicyRegistry::make_id(PolicyType::ALLOWLIST as u8, 999);
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
                    vec![a, PolicyRegistry::ALWAYS_ALLOW_ID],
                )
                .unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IPolicyRegistry::InvalidChildPolicy {
                    childPolicyId: PolicyRegistry::ALWAYS_ALLOW_ID,
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
            let missing = PolicyRegistry::make_id(PolicyType::ALLOWLIST as u8, 999);
            let err = LOGIC
                .create_composite_policy(
                    &mut rt,
                    ADMIN,
                    PolicyType::UNION,
                    vec![PolicyRegistry::ALWAYS_ALLOW_ID, missing],
                )
                .unwrap_err();
            assert_eq!(err, BasePrecompileError::revert(IPolicyRegistry::PolicyNotFound {}));
        }

        #[test]
        fn update_composite_nonexistent_reverts_policy_not_found() {
            let mut rt = initialized();
            let a = create_allowlist(&mut rt);
            let b = create_allowlist(&mut rt);
            let missing = PolicyRegistry::make_id(PolicyType::UNION as u8, 999);
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
                .update_composite(&mut rt, id, vec![a, PolicyRegistry::ALWAYS_BLOCK_ID])
                .unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IPolicyRegistry::InvalidChildPolicy {
                    childPolicyId: PolicyRegistry::ALWAYS_BLOCK_ID,
                })
            );
        }

        // --- inverted policy IDs (V3 / Denim) ---

        #[test]
        fn inverted_policy_id_toggles_bit_and_round_trips() {
            let policy_id = PolicyRegistry::make_id(PolicyType::ALLOWLIST as u8, 42);
            let inverted = PolicyRegistry::inverted_policy_id(policy_id);
            assert_eq!(inverted, policy_id ^ PolicyRegistry::INVERTED_POLICY_BIT);
            assert_eq!(PolicyRegistry::inverted_policy_id(inverted), policy_id);
        }

        #[test]
        fn inverted_unknown_policy_fails_closed() {
            let rt = initialized();
            let unknown_blocklist = PolicyRegistry::make_id(PolicyType::BLOCKLIST as u8, 999);
            assert!(is_authorized(&rt, unknown_blocklist, ALICE));
            assert!(!is_authorized(
                &rt,
                unknown_blocklist | PolicyRegistry::INVERTED_POLICY_BIT,
                ALICE,
            ));
        }

        #[test]
        fn inverted_policies_negate_simple_composite_and_builtin_authorization() {
            let mut rt = initialized();
            let allowlist = create_allowlist(&mut rt);
            LOGIC.update_allowlist(&mut rt, allowlist, true, vec![ALICE]).unwrap();
            let inverted_allowlist = allowlist | PolicyRegistry::INVERTED_POLICY_BIT;
            assert!(!is_authorized(&rt, inverted_allowlist, ALICE));
            assert!(is_authorized(&rt, inverted_allowlist, BOB));

            let other = create_allowlist(&mut rt);
            let composite = create_union(&mut rt, vec![allowlist, other]);
            assert!(is_authorized(&rt, composite, ALICE));
            assert!(!is_authorized(&rt, composite | PolicyRegistry::INVERTED_POLICY_BIT, ALICE,));
            assert!(!is_authorized(&rt, composite, BOB));
            assert!(is_authorized(&rt, composite | PolicyRegistry::INVERTED_POLICY_BIT, BOB,));

            assert!(!is_authorized(&rt, PolicyRegistry::INVERTED_POLICY_BIT, ALICE));
            assert!(is_authorized(
                &rt,
                PolicyRegistry::ALWAYS_BLOCK_ID | PolicyRegistry::INVERTED_POLICY_BIT,
                ALICE,
            ));
        }

        #[test]
        fn inverted_simple_children_are_valid_and_remain_verbatim() {
            let mut rt = initialized();
            let allowlist = create_allowlist(&mut rt);
            let exclusion = create_allowlist(&mut rt);
            let inverted_exclusion = exclusion | PolicyRegistry::INVERTED_POLICY_BIT;
            LOGIC.update_allowlist(&mut rt, allowlist, true, vec![ALICE]).unwrap();
            let composite = create_intersect(&mut rt, vec![allowlist, inverted_exclusion]);

            assert_eq!(
                LOGIC.composite_policy_child_ids(&rt, composite).unwrap(),
                vec![allowlist, inverted_exclusion],
            );
            assert_eq!(
                LOGIC
                    .composite_policy_child_ids(&rt, composite | PolicyRegistry::INVERTED_POLICY_BIT,)
                    .unwrap(),
                vec![allowlist, inverted_exclusion],
            );
            assert!(is_authorized(&rt, composite, ALICE));

            LOGIC.update_allowlist(&mut rt, exclusion, true, vec![ALICE]).unwrap();
            assert!(!is_authorized(&rt, composite, ALICE));
        }

        #[test]
        fn update_composite_accepts_an_inverted_simple_child() {
            let mut rt = initialized();
            let first = create_allowlist(&mut rt);
            let second = create_allowlist(&mut rt);
            let replacement = create_allowlist(&mut rt);
            let composite = create_union(&mut rt, vec![first, second]);
            let inverted_replacement = replacement | PolicyRegistry::INVERTED_POLICY_BIT;

            LOGIC.update_composite(&mut rt, composite, vec![first, inverted_replacement]).unwrap();
            assert_eq!(
                LOGIC.composite_policy_child_ids(&rt, composite).unwrap(),
                vec![first, inverted_replacement],
            );
        }

        #[test]
        fn inverted_composite_child_is_rejected_with_original_id() {
            let mut rt = initialized();
            let first = create_allowlist(&mut rt);
            let second = create_allowlist(&mut rt);
            let composite = create_union(&mut rt, vec![first, second]);
            let inverted_composite = composite | PolicyRegistry::INVERTED_POLICY_BIT;
            let third = create_allowlist(&mut rt);

            let err = LOGIC
                .create_composite_policy(
                    &mut rt,
                    ADMIN,
                    PolicyType::INTERSECT,
                    vec![third, inverted_composite],
                )
                .unwrap_err();
            assert_eq!(
                err,
                BasePrecompileError::revert(IPolicyRegistry::InvalidChildPolicy {
                    childPolicyId: inverted_composite,
                })
            );
        }

        #[test]
        fn inverted_getters_resolve_the_base_policy() {
            let mut rt = initialized();
            let policy_id = create_allowlist(&mut rt);
            let inverted = policy_id | PolicyRegistry::INVERTED_POLICY_BIT;
            LOGIC.stage_update_admin(&mut rt, policy_id, NEW_ADMIN).unwrap();

            assert_eq!(
                LOGIC.policy_exists(&rt, inverted).unwrap(),
                LOGIC.policy_exists(&rt, policy_id).unwrap()
            );
            assert_eq!(LOGIC.get_policy_admin(&rt, inverted).unwrap(), ADMIN);
            assert_eq!(LOGIC.pending_policy_admin(&rt, inverted).unwrap(), NEW_ADMIN);
        }

    }

}
