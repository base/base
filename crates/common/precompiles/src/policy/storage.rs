use alloc::vec::Vec;

use alloy_primitives::{Address, U256, address};
use base_precompile_macros::contract;
use base_precompile_storage::{BasePrecompileError, ContractStorage, Handler, Mapping, Result};

use super::{IPolicyRegistry, IPolicyRegistry::PolicyType};

/// A packed policy storage word.
///
/// Layout: `[255]` exists flag | `[254:160]` reserved (zero) | `[159:0]` admin (160 bits).
///
/// The policy type is not stored here — it is encoded in the high byte of the policy ID
/// and derived from there. Bit 255 is always set for any written slot, making the zero word
/// a reliable "never written" sentinel even when admin is `Address::ZERO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PackedPolicy(U256);

impl PackedPolicy {
    /// Bit 255: the highest bit of limb 3.
    const EXISTS_BIT: U256 = U256::from_limbs([0, 0, 0, 1u64 << 63]);
    /// Mask covering the low 160 bits where the admin address lives.
    const ADMIN_MASK: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0xFFFF_FFFF, 0]);

    fn new(admin: Address) -> Self {
        let mut word = [0u8; 32];
        word[12..32].copy_from_slice(admin.as_slice());
        Self(U256::from_be_slice(&word) | Self::EXISTS_BIT)
    }

    fn with_admin(self, new_admin: Address) -> Self {
        Self::new(new_admin)
    }

    fn admin(self) -> Address {
        let bytes = (self.0 & Self::ADMIN_MASK).to_be_bytes::<32>();
        Address::from_slice(&bytes[12..])
    }

    fn exists(self) -> bool {
        !(self.0 & Self::EXISTS_BIT).is_zero()
    }

    const fn into_u256(self) -> U256 {
        self.0
    }

    const fn from_raw(v: U256) -> Self {
        Self(v)
    }
}

/// Storage layout for the `PolicyRegistry` precompile.
///
/// Slots are append-only — never reorder across hardforks.
#[contract(addr = Self::ADDRESS)]
#[namespace("base.policy_registry")]
pub struct PolicyRegistryStorage {
    pub policies: Mapping<u64, U256>,                  // slot 0
    pub members: Mapping<u64, Mapping<Address, bool>>, // slot 1
    pub pending_admins: Mapping<u64, Address>,         // slot 2
    /// Global monotonic counter for the low 56 bits of all custom policy IDs.
    /// Intentionally shared across ALLOWLIST and BLOCKLIST types — the type
    /// discriminator is encoded in the top byte, so both types draw from the
    /// same 56-bit space without collision.
    pub next_counter: u64, // slot 3
}

impl PolicyRegistryStorage<'_> {
    /// Singleton precompile address for the `PolicyRegistry`.
    pub const ADDRESS: Address = address!("b030000000000000000000000000000000000000");

    /// Built-in policy ID that always authorizes every account.
    /// Encoded as BLOCKLIST (type=0) with counter=0 — an empty blocklist authorizes everyone.
    /// Also the EVM zero default: zero-initialized policy ID fields map here.
    pub const ALWAYS_ALLOW_ID: u64 = 0;

    /// Built-in policy ID that always rejects every account.
    /// Encoded as ALLOWLIST (type=1) with counter=1 and an empty member set,
    /// so no account is on the allowlist and nobody passes.
    pub const ALWAYS_BLOCK_ID: u64 = (1u64 << Self::POLICY_ID_TYPE_SHIFT) | 1;

    const ALLOWLIST_TYPE: u8 = PolicyType::ALLOWLIST as u8;
    const BLOCKLIST_TYPE: u8 = PolicyType::BLOCKLIST as u8;
    const COUNTER_MASK: u64 = (1u64 << 56) - 1;
    const POLICY_ID_TYPE_SHIFT: usize = 56;
    /// Number of built-in policies; the counter is set to this value after `write_builtins`.
    const BUILTIN_POLICY_COUNT: u64 = 2;

    fn require_write(&self) -> Result<()> {
        if self.storage.is_static() {
            return Err(BasePrecompileError::StaticCallViolation);
        }
        Ok(())
    }

    const fn policy_id_type(policy_id: u64) -> u8 {
        (policy_id >> Self::POLICY_ID_TYPE_SHIFT) as u8
    }

    fn require_custom(&self, policy_id: u64) -> Result<PackedPolicy> {
        let packed = PackedPolicy::from_raw(self.policies.at(&policy_id).read()?);
        if !packed.exists() {
            return Err(BasePrecompileError::revert(IPolicyRegistry::PolicyNotFound {}));
        }
        Ok(packed)
    }

    fn next_counter(&self) -> Result<u64> {
        self.next_counter.read()
    }

    const fn make_id(policy_type: u8, counter: u64) -> u64 {
        (policy_type as u64) << Self::POLICY_ID_TYPE_SHIFT | (counter & Self::COUNTER_MASK)
    }

    /// Validates the policy exists and the caller is its current admin.
    /// Returns `(packed, caller)` on success.
    fn require_admin(&self, policy_id: u64) -> Result<(PackedPolicy, Address)> {
        self.require_write()?;
        let packed = self.require_custom(policy_id)?;
        let caller = self.storage.caller();
        if packed.admin() != caller {
            return Err(BasePrecompileError::revert(IPolicyRegistry::Unauthorized {}));
        }
        Ok((packed, caller))
    }

    /// Writes the two built-in policies into the `policies` mapping.
    ///
    /// Consumes counters 0 and 1, leaving the counter at 2 so custom policies
    /// start there. Both built-ins have a renounced (zero) admin. Idempotent:
    /// if the counter is already past 0 the builtins were already written.
    /// - `ALWAYS_ALLOW_ID` (counter=0, BLOCKLIST): no members blocked — everyone is authorized.
    /// - `ALWAYS_BLOCK_ID` (counter=1, ALLOWLIST): no members allowed — nobody is authorized.
    pub fn write_builtins(&mut self) -> Result<()> {
        if self.next_counter.read()? >= Self::BUILTIN_POLICY_COUNT {
            return Ok(());
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
        let builtin = PackedPolicy::new(Address::ZERO).into_u256();
        self.policies.at_mut(&Self::ALWAYS_ALLOW_ID).write(builtin)?;
        self.policies.at_mut(&Self::ALWAYS_BLOCK_ID).write(builtin)?;
        self.next_counter.write(Self::BUILTIN_POLICY_COUNT)?;
        Ok(())
    }

    /// Creates a new ALLOWLIST or BLOCKLIST policy, returning its encoded ID.
    pub fn create_policy(&mut self, admin: Address, policy_type: PolicyType) -> Result<u64> {
        self.require_write()?;
        let policy_type_u8 = policy_type.as_discriminant();
        if admin == Address::ZERO {
            return Err(BasePrecompileError::revert(IPolicyRegistry::ZeroAddress {}));
        }

        // The registry account must be non-empty before the first policy storage write; otherwise
        // the EVM path can prune writes made under an empty native-precompile account.
        // TODO: Revisit this guard against the finalized Beryl gas model, since `is_initialized`
        // charges warm/cold account-read gas before skipping repeated `set_code`.
        if !self.is_initialized()? {
            self.__initialize()?;
            self.write_builtins()?;
        }

        let counter = self.next_counter()?;
        let next = counter.checked_add(1).ok_or_else(BasePrecompileError::under_overflow)?;
        self.next_counter.write(next)?;
        let policy_id = Self::make_id(policy_type_u8, counter);
        self.policies.at_mut(&policy_id).write(PackedPolicy::new(admin).into_u256())?;

        let caller = self.storage.caller();
        self.emit_event(IPolicyRegistry::PolicyCreated {
            policyId: policy_id,
            creator: caller,
            policyType: policy_type,
        })?;
        self.emit_event(IPolicyRegistry::PolicyAdminUpdated {
            policyId: policy_id,
            previousAdmin: Address::ZERO,
            newAdmin: admin,
        })?;

        Ok(policy_id)
    }

    /// Creates a new policy and populates its initial member list.
    pub fn create_policy_with_accounts(
        &mut self,
        admin: Address,
        policy_type: PolicyType,
        accounts: Vec<Address>,
    ) -> Result<u64> {
        let policy_id = self.create_policy(admin, policy_type)?;
        let caller = self.storage.caller();
        for account in &accounts {
            self.members.at_mut(&policy_id).at_mut(account).write(true)?;
        }
        match policy_type {
            PolicyType::ALLOWLIST => self.emit_event(IPolicyRegistry::AllowlistUpdated {
                policyId: policy_id,
                updater: caller,
                allowed: true,
                accounts,
            })?,
            PolicyType::BLOCKLIST => self.emit_event(IPolicyRegistry::BlocklistUpdated {
                policyId: policy_id,
                updater: caller,
                blocked: true,
                accounts,
            })?,
            _ => return Err(BasePrecompileError::enum_conversion_error()),
        }
        Ok(policy_id)
    }

    /// Stages `new_admin` as the pending admin for `policy_id`.
    ///
    /// Passing `address(0)` clears a previously-staged transfer per the interface spec.
    pub fn stage_update_admin(&mut self, policy_id: u64, new_admin: Address) -> Result<()> {
        let (_, caller) = self.require_admin(policy_id)?;
        if new_admin == Address::ZERO {
            self.pending_admins.at_mut(&policy_id).delete()?;
        } else {
            self.pending_admins.at_mut(&policy_id).write(new_admin)?;
        }
        self.emit_event(IPolicyRegistry::PolicyAdminStaged {
            policyId: policy_id,
            currentAdmin: caller,
            pendingAdmin: new_admin,
        })?;
        Ok(())
    }

    /// Completes a pending admin transfer; caller must be the staged pending admin.
    pub fn finalize_update_admin(&mut self, policy_id: u64) -> Result<()> {
        self.require_write()?;
        let packed = self.require_custom(policy_id)?;
        let pending = self.pending_admins.at(&policy_id).read()?;
        if pending == Address::ZERO {
            return Err(BasePrecompileError::revert(IPolicyRegistry::NoPendingAdmin {}));
        }
        let caller = self.storage.caller();
        if pending != caller {
            return Err(BasePrecompileError::revert(IPolicyRegistry::Unauthorized {}));
        }
        let previous_admin = packed.admin();
        self.policies.at_mut(&policy_id).write(packed.with_admin(caller).into_u256())?;
        self.pending_admins.at_mut(&policy_id).delete()?;
        self.emit_event(IPolicyRegistry::PolicyAdminUpdated {
            policyId: policy_id,
            previousAdmin: previous_admin,
            newAdmin: caller,
        })?;
        Ok(())
    }

    /// Clears the admin of `policy_id`, leaving it permanently un-administered.
    pub fn renounce_admin(&mut self, policy_id: u64) -> Result<()> {
        let (packed, caller) = self.require_admin(policy_id)?;
        self.policies.at_mut(&policy_id).write(packed.with_admin(Address::ZERO).into_u256())?;
        self.pending_admins.at_mut(&policy_id).delete()?;
        self.emit_event(IPolicyRegistry::PolicyAdminUpdated {
            policyId: policy_id,
            previousAdmin: caller,
            newAdmin: Address::ZERO,
        })?;
        Ok(())
    }

    /// Adds or removes `accounts` from the allowlist for an ALLOWLIST policy.
    pub fn update_allowlist(
        &mut self,
        policy_id: u64,
        allowed: bool,
        accounts: Vec<Address>,
    ) -> Result<()> {
        let caller = self.update_membership(policy_id, Self::ALLOWLIST_TYPE, allowed, &accounts)?;
        self.emit_event(IPolicyRegistry::AllowlistUpdated {
            policyId: policy_id,
            updater: caller,
            allowed,
            accounts,
        })
    }

    /// Adds or removes `accounts` from the blocklist for a BLOCKLIST policy.
    pub fn update_blocklist(
        &mut self,
        policy_id: u64,
        blocked: bool,
        accounts: Vec<Address>,
    ) -> Result<()> {
        let caller = self.update_membership(policy_id, Self::BLOCKLIST_TYPE, blocked, &accounts)?;
        self.emit_event(IPolicyRegistry::BlocklistUpdated {
            policyId: policy_id,
            updater: caller,
            blocked,
            accounts,
        })
    }

    fn update_membership(
        &mut self,
        policy_id: u64,
        expected_type: u8,
        add: bool,
        accounts: &[Address],
    ) -> Result<Address> {
        let (_, caller) = self.require_admin(policy_id)?;
        if Self::policy_id_type(policy_id) != expected_type {
            return Err(BasePrecompileError::revert(IPolicyRegistry::IncompatiblePolicyType {}));
        }
        for account in accounts {
            if add {
                self.members.at_mut(&policy_id).at_mut(account).write(true)?;
            } else {
                self.members.at_mut(&policy_id).at_mut(account).delete()?;
            }
        }
        Ok(caller)
    }

    /// Returns `true` if `account` is authorized under `policy_id`.
    ///
    /// Malformed policy IDs (type byte > 1) return `Ok(false)` rather than reverting.
    ///
    /// If the policy slot has never been written, the function falls back to default
    /// semantics for that type: an ALLOWLIST with no members authorizes nobody (`false`),
    /// a BLOCKLIST with no members blocks nobody (`true`). `PolicyNotFound` is never returned.
    pub fn is_authorized(&self, policy_id: u64, account: Address) -> Result<bool> {
        // Malformed IDs (type byte > 1) are treated as unauthorized rather than reverting.
        if Self::policy_id_type(policy_id) > PolicyType::ALLOWLIST as u8 {
            return Ok(false);
        }
        // Fast-paths for built-in IDs: ALWAYS_ALLOW_ID = 0 is the EVM default for any
        // uninitialized policy field, so this must work before write_builtins() has run.
        if policy_id == Self::ALWAYS_ALLOW_ID {
            return Ok(true);
        }
        if policy_id == Self::ALWAYS_BLOCK_ID {
            return Ok(false);
        }
        // Read membership directly without requiring the policy slot to be written first.
        // If the slot is unwritten the mapping returns false, which naturally gives:
        //   ALLOWLIST  => false  (no members => not authorized)
        //   BLOCKLIST  => !false (no members blocked => authorized)
        let member = self.members.at(&policy_id).at(&account).read()?;
        match Self::policy_id_type(policy_id) {
            Self::ALLOWLIST_TYPE => Ok(member),
            Self::BLOCKLIST_TYPE => Ok(!member),
            _ => unreachable!("type byte > 1 was rejected by the malformed-ID guard above"),
        }
    }

    /// Returns `true` if `policy_id` refers to an existing policy.
    ///
    /// Malformed policy IDs (type byte > 1) return `Ok(false)` rather than reverting.
    /// Built-in IDs always return `true` via a fast-path, without reading storage.
    /// This is necessary because `ALWAYS_ALLOW_ID = 0` is the EVM default for any
    /// uninitialized policy field, so it must be recognized as valid before
    /// `write_builtins` has run.
    pub fn policy_exists(&self, policy_id: u64) -> Result<bool> {
        // Malformed IDs (type byte > 1) are not well-formed, so they do not exist.
        if Self::policy_id_type(policy_id) > PolicyType::ALLOWLIST as u8 {
            return Ok(false);
        }
        if policy_id == Self::ALWAYS_ALLOW_ID || policy_id == Self::ALWAYS_BLOCK_ID {
            return Ok(true);
        }
        let packed = PackedPolicy::from_raw(self.policies.at(&policy_id).read()?);
        Ok(packed.exists())
    }

    /// Returns the current admin of `policy_id`, or `address(0)` for policies with renounced admin.
    ///
    /// Returns `address(0)` without reverting for malformed policy IDs (type byte > 1) and for
    /// policy IDs that have never been written to storage.
    pub fn get_policy_admin(&self, policy_id: u64) -> Result<Address> {
        if Self::policy_id_type(policy_id) > PolicyType::ALLOWLIST as u8 {
            return Ok(Address::ZERO);
        }
        let packed = PackedPolicy::from_raw(self.policies.at(&policy_id).read()?);
        if !packed.exists() {
            return Ok(Address::ZERO);
        }
        Ok(packed.admin())
    }

    /// Returns the pending admin staged for `policy_id`, or `address(0)` if none.
    ///
    /// Returns `address(0)` without reverting for malformed policy IDs (type byte > 1). For
    /// policy IDs that exist but have no pending transfer, the storage slot returns `address(0)`
    /// naturally.
    pub fn pending_policy_admin(&self, policy_id: u64) -> Result<Address> {
        if Self::policy_id_type(policy_id) > PolicyType::ALLOWLIST as u8 {
            return Ok(Address::ZERO);
        }
        self.pending_admins.at(&policy_id).read()
    }
}

impl crate::Policy for PolicyRegistryStorage<'_> {
    fn is_authorized(&self, policy_id: u64, account: Address) -> Result<bool> {
        PolicyRegistryStorage::is_authorized(self, policy_id, account)
    }

    fn policy_exists(&self, policy_id: u64) -> Result<bool> {
        PolicyRegistryStorage::policy_exists(self, policy_id)
    }
}

impl crate::PolicyRegistry for PolicyRegistryStorage<'_> {
    fn create_policy(&mut self, admin: Address, policy_type: PolicyType) -> Result<u64> {
        PolicyRegistryStorage::create_policy(self, admin, policy_type)
    }

    fn create_policy_with_accounts(
        &mut self,
        admin: Address,
        policy_type: PolicyType,
        accounts: alloc::vec::Vec<Address>,
    ) -> Result<u64> {
        PolicyRegistryStorage::create_policy_with_accounts(self, admin, policy_type, accounts)
    }

    fn stage_update_admin(&mut self, policy_id: u64, new_admin: Address) -> Result<()> {
        PolicyRegistryStorage::stage_update_admin(self, policy_id, new_admin)
    }

    fn finalize_update_admin(&mut self, policy_id: u64) -> Result<()> {
        PolicyRegistryStorage::finalize_update_admin(self, policy_id)
    }

    fn renounce_admin(&mut self, policy_id: u64) -> Result<()> {
        PolicyRegistryStorage::renounce_admin(self, policy_id)
    }

    fn update_allowlist(
        &mut self,
        policy_id: u64,
        allowed: bool,
        accounts: alloc::vec::Vec<Address>,
    ) -> Result<()> {
        PolicyRegistryStorage::update_allowlist(self, policy_id, allowed, accounts)
    }

    fn update_blocklist(
        &mut self,
        policy_id: u64,
        blocked: bool,
        accounts: alloc::vec::Vec<Address>,
    ) -> Result<()> {
        PolicyRegistryStorage::update_blocklist(self, policy_id, blocked, accounts)
    }

    fn get_policy_admin(&self, policy_id: u64) -> Result<Address> {
        PolicyRegistryStorage::get_policy_admin(self, policy_id)
    }

    fn pending_policy_admin(&self, policy_id: u64) -> Result<Address> {
        PolicyRegistryStorage::pending_policy_admin(self, policy_id)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256, address, uint};
    use alloy_sol_types::SolEvent;
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx, StorageKey};

    use super::*;
    use crate::IPolicyRegistry;

    // --- PackedPolicy unit tests ---

    #[test]
    fn packed_policy_new_roundtrips_admin() {
        let p = PackedPolicy::new(ADMIN);
        assert_eq!(p.admin(), ADMIN);
        assert!(p.exists());
    }

    #[test]
    fn packed_policy_zero_signals_never_created() {
        let p = PackedPolicy::from_raw(U256::ZERO);
        assert!(!p.exists());
    }

    #[test]
    fn packed_policy_zero_admin_is_non_zero() {
        // Exists flag at bit 255 keeps the word non-zero even with zero admin.
        let p = PackedPolicy::new(Address::ZERO);
        assert!(p.exists());
        assert_eq!(p.admin(), Address::ZERO);
    }

    #[test]
    fn packed_policy_into_u256_from_raw_roundtrip() {
        let p = PackedPolicy::new(ADMIN);
        let p2 = PackedPolicy::from_raw(p.into_u256());
        assert_eq!(p, p2);
        assert_eq!(p2.admin(), ADMIN);
    }

    #[test]
    fn packed_policy_different_admins_produce_different_words() {
        let other = address!("0x2000000000000000000000000000000000000002");
        assert_ne!(PackedPolicy::new(ADMIN), PackedPolicy::new(other));
    }

    const ADMIN: Address = address!("0x1000000000000000000000000000000000000001");
    const ALICE: Address = address!("0xA000000000000000000000000000000000000001");
    const BOB: Address = address!("0xB000000000000000000000000000000000000001");
    const NEW_ADMIN: Address = address!("0x2000000000000000000000000000000000000002");
    const POLICY_REGISTRY_ROOT: U256 =
        uint!(0x00503aeb06982fa1fe3151dc68f90b3946c55c449dfd447e49dcaece71ba4a00_U256);

    /// Returns a storage provider with both built-in policies pre-written.
    fn storage() -> HashMapStorageProvider {
        let mut s = HashMapStorageProvider::new(1);
        s.set_caller(ADMIN);
        StorageCtx::enter(&mut s, |ctx| PolicyRegistryStorage::new(ctx).write_builtins()).unwrap();
        s
    }

    fn create_allowlist(s: &mut HashMapStorageProvider) -> u64 {
        StorageCtx::enter(s, |ctx| {
            PolicyRegistryStorage::new(ctx).create_policy(ADMIN, PolicyType::ALLOWLIST)
        })
        .unwrap()
    }

    fn create_blocklist(s: &mut HashMapStorageProvider) -> u64 {
        StorageCtx::enter(s, |ctx| {
            PolicyRegistryStorage::new(ctx).create_policy(ADMIN, PolicyType::BLOCKLIST)
        })
        .unwrap()
    }

    fn is_authorized(s: &mut HashMapStorageProvider, policy_id: u64, account: Address) -> bool {
        StorageCtx::enter(s, |ctx| {
            PolicyRegistryStorage::new(ctx).is_authorized(policy_id, account)
        })
        .unwrap()
    }

    #[test]
    fn policy_registry_namespace_matches_base_std_root() {
        assert_eq!(slots::POLICIES, POLICY_REGISTRY_ROOT);
        assert_eq!(slots::MEMBERS, POLICY_REGISTRY_ROOT + U256::from(1));
        assert_eq!(slots::PENDING_ADMINS, POLICY_REGISTRY_ROOT + U256::from(2));
        assert_eq!(slots::NEXT_COUNTER, POLICY_REGISTRY_ROOT + U256::from(3));
    }

    #[test]
    fn policy_registry_writes_use_base_std_namespace_slots() {
        let mut s = storage();
        let id = create_allowlist(&mut s);

        StorageCtx::enter(&mut s, |ctx| {
            assert_ne!(
                ctx.sload(PolicyRegistryStorage::ADDRESS, id.mapping_slot(slots::POLICIES))
                    .unwrap(),
                U256::ZERO
            );
            assert_eq!(
                ctx.sload(PolicyRegistryStorage::ADDRESS, slots::NEXT_COUNTER).unwrap(),
                U256::from(3)
            );
            assert_eq!(
                ctx.sload(PolicyRegistryStorage::ADDRESS, id.mapping_slot(U256::ZERO)).unwrap(),
                U256::ZERO
            );
        });
    }

    // --- built-in IDs ---

    #[test]
    fn always_allow_id_authorizes_any_account() {
        let mut s = storage();
        assert!(is_authorized(&mut s, PolicyRegistryStorage::ALWAYS_ALLOW_ID, ALICE));
        assert!(is_authorized(&mut s, PolicyRegistryStorage::ALWAYS_ALLOW_ID, BOB));
    }

    #[test]
    fn always_block_id_rejects_any_account() {
        let mut s = storage();
        assert!(!is_authorized(&mut s, PolicyRegistryStorage::ALWAYS_BLOCK_ID, ALICE));
        assert!(!is_authorized(&mut s, PolicyRegistryStorage::ALWAYS_BLOCK_ID, BOB));
    }

    #[test]
    fn unknown_blocklist_policy_id_authorizes_account() {
        // 0xdeadbeef has type byte 0 (BLOCKLIST); no members blocked => authorized.
        let mut s = storage();
        assert!(is_authorized(&mut s, 0xdeadbeef, ALICE));
    }

    #[test]
    fn unknown_allowlist_policy_id_does_not_authorize_account() {
        // A well-formed ALLOWLIST ID that was never written to storage.
        // No members exist => not authorized.
        let unknown_allowlist = PolicyRegistryStorage::make_id(PolicyType::ALLOWLIST as u8, 9999);
        let mut s = storage();
        assert!(!is_authorized(&mut s, unknown_allowlist, ALICE));
    }

    #[test]
    fn malformed_policy_id_is_authorized_returns_false() {
        // Type byte > 1 => malformed; is_authorized must return false, not revert.
        let malformed: u64 = (2u64 << 56) | 42;
        let mut s = storage();
        let result = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).is_authorized(malformed, ALICE)
        })
        .unwrap();
        assert!(!result);
    }

    #[test]
    fn malformed_policy_id_policy_exists_returns_false() {
        // Type byte > 1 => malformed; policy_exists must return false, not revert.
        let malformed: u64 = (5u64 << 56) | 100;
        let mut s = storage();
        let result = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).policy_exists(malformed)
        })
        .unwrap();
        assert!(!result);
    }

    // --- write_builtins initialization ---

    #[test]
    fn first_create_policy_initializes_builtins_and_starts_counter_at_two() {
        // Start from bare storage — write_builtins has NOT been called yet.
        let mut s = HashMapStorageProvider::new(1);
        s.set_caller(ADMIN);
        let id = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).create_policy(ADMIN, PolicyType::ALLOWLIST)
        })
        .unwrap();
        // Builtins claimed counters 0 and 1; first custom policy gets 2.
        assert_eq!(id & PolicyRegistryStorage::COUNTER_MASK, 2);
        // Builtins are now in storage.
        assert!(
            StorageCtx::enter(&mut s, |ctx| PolicyRegistryStorage::new(ctx)
                .policy_exists(PolicyRegistryStorage::ALWAYS_ALLOW_ID))
            .unwrap()
        );
        assert!(
            StorageCtx::enter(&mut s, |ctx| PolicyRegistryStorage::new(ctx)
                .policy_exists(PolicyRegistryStorage::ALWAYS_BLOCK_ID))
            .unwrap()
        );
    }

    #[test]
    fn write_builtins_is_idempotent() {
        let mut s = HashMapStorageProvider::new(1);
        for _ in 0..3 {
            StorageCtx::enter(&mut s, |ctx| PolicyRegistryStorage::new(ctx).write_builtins())
                .unwrap();
        }
        // Counter must be exactly BUILTIN_POLICY_COUNT regardless of how many times called.
        let counter =
            StorageCtx::enter(&mut s, |ctx| PolicyRegistryStorage::new(ctx).next_counter.read())
                .unwrap();
        assert_eq!(counter, PolicyRegistryStorage::BUILTIN_POLICY_COUNT);
    }

    // --- createPolicy ---

    #[test]
    fn create_policy_zero_admin_reverts() {
        let mut s = storage();
        let err = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).create_policy(Address::ZERO, PolicyType::ALLOWLIST)
        })
        .unwrap_err();
        assert!(matches!(err, BasePrecompileError::Revert(_)));
    }

    #[test]
    fn create_policy_ids_encode_type_in_top_byte_and_increment_counter() {
        let mut s = storage();
        let id1 = create_allowlist(&mut s);
        let id2 = create_blocklist(&mut s);
        assert_eq!((id1 >> 56) as u8, PolicyType::ALLOWLIST as u8);
        assert_eq!((id2 >> 56) as u8, PolicyType::BLOCKLIST as u8);
        assert_eq!(id1 & PolicyRegistryStorage::COUNTER_MASK, 2);
        assert_eq!(id2 & PolicyRegistryStorage::COUNTER_MASK, 3);
    }

    #[test]
    fn create_policy_emits_policy_created_and_admin_updated_events() {
        let mut s = storage();
        let id = create_allowlist(&mut s);
        let events = s.get_events(PolicyRegistryStorage::ADDRESS);
        assert_eq!(events.len(), 2);
        let created = IPolicyRegistry::PolicyCreated::decode_log_data(&events[0]).unwrap();
        assert_eq!(created.policyId, id);
        assert_eq!(created.creator, ADMIN);
        assert_eq!(created.policyType, PolicyType::ALLOWLIST);
    }

    #[test]
    fn update_allowlist_emits_allowlist_updated_event() {
        let mut s = storage();
        let id = create_allowlist(&mut s);
        s.set_caller(ADMIN);
        StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).update_allowlist(id, true, vec![ALICE])
        })
        .unwrap();
        let events = s.get_events(PolicyRegistryStorage::ADDRESS);
        let updated =
            IPolicyRegistry::AllowlistUpdated::decode_log_data(events.last().unwrap()).unwrap();
        assert_eq!(updated.policyId, id);
        assert_eq!(updated.updater, ADMIN);
        assert!(updated.allowed);
        assert_eq!(updated.accounts, vec![ALICE]);
    }

    // --- ALLOWLIST membership ---

    #[test]
    fn allowlist_non_member_is_not_authorized() {
        let mut s = storage();
        let id = create_allowlist(&mut s);
        assert!(!is_authorized(&mut s, id, ALICE));
    }

    #[test]
    fn allowlist_add_then_remove_member() {
        let mut s = storage();
        let id = create_allowlist(&mut s);

        StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).update_allowlist(id, true, vec![ALICE])
        })
        .unwrap();
        assert!(is_authorized(&mut s, id, ALICE));

        StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).update_allowlist(id, false, vec![ALICE])
        })
        .unwrap();
        assert!(!is_authorized(&mut s, id, ALICE));
    }

    #[test]
    fn allowlist_batch_update_flips_all_accounts() {
        let mut s = storage();
        let id = create_allowlist(&mut s);

        StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).update_allowlist(id, true, vec![ALICE, BOB])
        })
        .unwrap();
        assert!(is_authorized(&mut s, id, ALICE));
        assert!(is_authorized(&mut s, id, BOB));

        StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).update_allowlist(id, false, vec![ALICE, BOB])
        })
        .unwrap();
        assert!(!is_authorized(&mut s, id, ALICE));
        assert!(!is_authorized(&mut s, id, BOB));
    }

    #[test]
    fn allowlist_readding_existing_member_is_idempotent() {
        let mut s = storage();
        let id = create_allowlist(&mut s);
        for _ in 0..2 {
            StorageCtx::enter(&mut s, |ctx| {
                PolicyRegistryStorage::new(ctx).update_allowlist(id, true, vec![ALICE])
            })
            .unwrap();
        }
        assert!(is_authorized(&mut s, id, ALICE));
    }

    #[test]
    fn allowlist_removing_non_member_is_idempotent() {
        let mut s = storage();
        let id = create_allowlist(&mut s);
        StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).update_allowlist(id, false, vec![ALICE])
        })
        .unwrap();
        assert!(!is_authorized(&mut s, id, ALICE));
    }

    #[test]
    fn update_allowlist_on_blocklist_policy_reverts() {
        let mut s = storage();
        let id = create_blocklist(&mut s);
        let err = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).update_allowlist(id, true, vec![ALICE])
        })
        .unwrap_err();
        assert!(matches!(err, BasePrecompileError::Revert(_)));
    }

    // --- BLOCKLIST membership ---

    #[test]
    fn blocklist_non_member_is_authorized() {
        let mut s = storage();
        let id = create_blocklist(&mut s);
        assert!(is_authorized(&mut s, id, ALICE));
    }

    #[test]
    fn blocklist_block_then_unblock_member() {
        let mut s = storage();
        let id = create_blocklist(&mut s);

        StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).update_blocklist(id, true, vec![ALICE])
        })
        .unwrap();
        assert!(!is_authorized(&mut s, id, ALICE));

        StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).update_blocklist(id, false, vec![ALICE])
        })
        .unwrap();
        assert!(is_authorized(&mut s, id, ALICE));
    }

    #[test]
    fn update_blocklist_on_allowlist_policy_reverts() {
        let mut s = storage();
        let id = create_allowlist(&mut s);
        let err = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).update_blocklist(id, true, vec![ALICE])
        })
        .unwrap_err();
        assert!(matches!(err, BasePrecompileError::Revert(_)));
    }

    // --- createPolicyWithAccounts ---

    #[test]
    fn create_policy_with_accounts_seeds_members() {
        let mut s = storage();
        let id = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).create_policy_with_accounts(
                ADMIN,
                PolicyType::ALLOWLIST,
                vec![ALICE, BOB],
            )
        })
        .unwrap();
        assert!(is_authorized(&mut s, id, ALICE));
        assert!(is_authorized(&mut s, id, BOB));
    }

    #[test]
    fn create_policy_with_accounts_empty_batch_emits_seed_event() {
        let mut s = storage();
        let id = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).create_policy_with_accounts(
                ADMIN,
                PolicyType::ALLOWLIST,
                Vec::new(),
            )
        })
        .unwrap();

        let events = s.get_events(PolicyRegistryStorage::ADDRESS);
        assert_eq!(events.len(), 3);
        let updated =
            IPolicyRegistry::AllowlistUpdated::decode_log_data(events.last().unwrap()).unwrap();
        assert_eq!(updated.policyId, id);
        assert_eq!(updated.updater, ADMIN);
        assert!(updated.allowed);
        assert!(updated.accounts.is_empty());
    }

    // --- two-step admin transfer ---

    #[test]
    fn admin_transfer_two_step() {
        let mut s = storage();
        let id = create_allowlist(&mut s);

        StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).stage_update_admin(id, NEW_ADMIN)
        })
        .unwrap();

        s.set_caller(NEW_ADMIN);
        StorageCtx::enter(&mut s, |ctx| PolicyRegistryStorage::new(ctx).finalize_update_admin(id))
            .unwrap();

        s.set_caller(NEW_ADMIN);
        StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).update_allowlist(id, true, vec![ALICE])
        })
        .unwrap();
        assert!(is_authorized(&mut s, id, ALICE));
    }

    #[test]
    fn finalize_update_admin_without_pending_reverts() {
        let mut s = storage();
        let id = create_allowlist(&mut s);
        let err = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).finalize_update_admin(id)
        })
        .unwrap_err();
        assert!(matches!(err, BasePrecompileError::Revert(_)));
    }

    // --- renounceAdmin ---

    #[test]
    fn renounce_admin_freezes_policy() {
        let mut s = storage();
        let id = create_allowlist(&mut s);

        StorageCtx::enter(&mut s, |ctx| PolicyRegistryStorage::new(ctx).renounce_admin(id))
            .unwrap();

        let err = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).update_allowlist(id, true, vec![ALICE])
        })
        .unwrap_err();
        assert!(matches!(err, BasePrecompileError::Revert(_)));
    }

    // --- static call guard ---

    #[test]
    fn write_in_static_context_reverts() {
        let mut s = storage();
        s.set_static(true);
        let err = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).create_policy(ADMIN, PolicyType::ALLOWLIST)
        })
        .unwrap_err();
        assert_eq!(err, BasePrecompileError::StaticCallViolation);
    }

    // --- create_policy_with_accounts edge cases ---

    #[test]
    fn create_policy_with_accounts_blocklist_seeds_blocked_members() {
        let mut s = storage();
        let id = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).create_policy_with_accounts(
                ADMIN,
                PolicyType::BLOCKLIST,
                vec![ALICE, BOB],
            )
        })
        .unwrap();
        assert!(!is_authorized(&mut s, id, ALICE));
        assert!(!is_authorized(&mut s, id, BOB));
    }

    // --- stage_update_admin authorization ---

    #[test]
    fn stage_update_admin_unauthorized_reverts() {
        let mut s = storage();
        let id = create_allowlist(&mut s);
        s.set_caller(ALICE);
        let err = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).stage_update_admin(id, NEW_ADMIN)
        })
        .unwrap_err();
        assert!(matches!(err, BasePrecompileError::Revert(_)));
    }

    // --- finalize_update_admin authorization ---

    #[test]
    fn finalize_update_admin_unauthorized_reverts() {
        let mut s = storage();
        let id = create_allowlist(&mut s);
        StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).stage_update_admin(id, NEW_ADMIN)
        })
        .unwrap();
        s.set_caller(ALICE);
        let err = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).finalize_update_admin(id)
        })
        .unwrap_err();
        assert!(matches!(err, BasePrecompileError::Revert(_)));
    }

    // --- renounce_admin authorization ---

    #[test]
    fn renounce_admin_unauthorized_reverts() {
        let mut s = storage();
        let id = create_allowlist(&mut s);
        s.set_caller(ALICE);
        let err =
            StorageCtx::enter(&mut s, |ctx| PolicyRegistryStorage::new(ctx).renounce_admin(id))
                .unwrap_err();
        assert!(matches!(err, BasePrecompileError::Revert(_)));
    }

    // --- update_allowlist static call ---

    #[test]
    fn update_allowlist_static_call_reverts() {
        let mut s = storage();
        let id = create_allowlist(&mut s);
        s.set_static(true);
        let err = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).update_allowlist(id, true, vec![ALICE])
        })
        .unwrap_err();
        assert_eq!(err, BasePrecompileError::StaticCallViolation);
    }

    // --- policy_exists for built-in IDs ---

    #[test]
    fn policy_exists_builtin_ids_always_return_true() {
        let mut s = HashMapStorageProvider::new(1);
        assert!(
            StorageCtx::enter(&mut s, |ctx| {
                PolicyRegistryStorage::new(ctx)
                    .policy_exists(PolicyRegistryStorage::ALWAYS_ALLOW_ID)
            })
            .unwrap()
        );
        assert!(
            StorageCtx::enter(&mut s, |ctx| {
                PolicyRegistryStorage::new(ctx)
                    .policy_exists(PolicyRegistryStorage::ALWAYS_BLOCK_ID)
            })
            .unwrap()
        );
    }

    // --- get_policy_admin for built-in IDs ---

    #[test]
    fn get_policy_admin_builtin_ids_return_zero_address() {
        let mut s = storage();
        assert_eq!(
            StorageCtx::enter(&mut s, |ctx| {
                PolicyRegistryStorage::new(ctx)
                    .get_policy_admin(PolicyRegistryStorage::ALWAYS_ALLOW_ID)
            })
            .unwrap(),
            Address::ZERO
        );
        assert_eq!(
            StorageCtx::enter(&mut s, |ctx| {
                PolicyRegistryStorage::new(ctx)
                    .get_policy_admin(PolicyRegistryStorage::ALWAYS_BLOCK_ID)
            })
            .unwrap(),
            Address::ZERO
        );
    }

    // --- pending_policy_admin for built-in IDs and unknown IDs ---

    #[test]
    fn pending_policy_admin_builtin_ids_return_zero_address() {
        let mut s = storage();
        assert_eq!(
            StorageCtx::enter(&mut s, |ctx| {
                PolicyRegistryStorage::new(ctx)
                    .pending_policy_admin(PolicyRegistryStorage::ALWAYS_ALLOW_ID)
            })
            .unwrap(),
            Address::ZERO
        );
        assert_eq!(
            StorageCtx::enter(&mut s, |ctx| {
                PolicyRegistryStorage::new(ctx)
                    .pending_policy_admin(PolicyRegistryStorage::ALWAYS_BLOCK_ID)
            })
            .unwrap(),
            Address::ZERO
        );
    }

    #[test]
    fn pending_policy_admin_unknown_id_returns_zero_address() {
        let mut s = storage();
        let pending = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).pending_policy_admin(0xdeadbeef)
        })
        .unwrap();
        assert_eq!(pending, Address::ZERO);
    }

    // A policy ID whose type byte is 2 (> ALLOWLIST=1) is malformed.
    const MALFORMED_POLICY_ID: u64 = (2u64 << 56) | 42;

    #[test]
    fn get_policy_admin_malformed_policy_id_returns_zero_address() {
        let mut s = storage();
        let admin = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).get_policy_admin(MALFORMED_POLICY_ID)
        })
        .unwrap();
        assert_eq!(admin, Address::ZERO);
    }

    #[test]
    fn get_policy_admin_nonexistent_policy_returns_zero_address() {
        let mut s = storage();
        // 0xdeadbeef has type byte 0, so it is well-formed but was never created.
        let admin = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).get_policy_admin(0xdeadbeef)
        })
        .unwrap();
        assert_eq!(admin, Address::ZERO);
    }

    #[test]
    fn pending_policy_admin_malformed_policy_id_returns_zero_address() {
        let mut s = storage();
        let pending = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).pending_policy_admin(MALFORMED_POLICY_ID)
        })
        .unwrap();
        assert_eq!(pending, Address::ZERO);
    }

    #[test]
    fn pending_policy_admin_nonexistent_well_formed_policy_returns_zero_address() {
        // A well-formed ID (type byte in range) that was never created: storage
        // slot is unwritten, so the read returns Address::ZERO without reverting.
        let mut s = storage();
        let nonexistent = PolicyRegistryStorage::make_id(0, 999);
        let pending = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).pending_policy_admin(nonexistent)
        })
        .unwrap();
        assert_eq!(pending, Address::ZERO);
    }

    // --- builtin policies block mutations via Unauthorized ---

    #[test]
    fn builtin_policies_reject_admin_mutations() {
        let mut s = storage();
        // Both built-in policies have zero admin, so any caller gets Unauthorized.
        for policy_id in
            [PolicyRegistryStorage::ALWAYS_ALLOW_ID, PolicyRegistryStorage::ALWAYS_BLOCK_ID]
        {
            let err = StorageCtx::enter(&mut s, |ctx| {
                PolicyRegistryStorage::new(ctx).stage_update_admin(policy_id, ALICE)
            })
            .unwrap_err();
            assert!(matches!(err, BasePrecompileError::Revert(_)));
        }
    }

    // --- PolicyRegistryTrait delegation ---

    #[test]
    fn trait_create_policy_delegates() {
        let mut s = storage();
        let id = StorageCtx::enter(&mut s, |ctx| {
            let mut reg = PolicyRegistryStorage::new(ctx);
            crate::PolicyRegistry::create_policy(&mut reg, ADMIN, PolicyType::ALLOWLIST)
        })
        .unwrap();
        assert_eq!((id >> 56) as u8, PolicyType::ALLOWLIST as u8);
    }

    #[test]
    fn trait_create_policy_with_accounts_delegates() {
        let mut s = storage();
        let id = StorageCtx::enter(&mut s, |ctx| {
            let mut reg = PolicyRegistryStorage::new(ctx);
            crate::PolicyRegistry::create_policy_with_accounts(
                &mut reg,
                ADMIN,
                PolicyType::ALLOWLIST,
                vec![ALICE],
            )
        })
        .unwrap();
        assert!(is_authorized(&mut s, id, ALICE));
    }

    #[test]
    fn trait_stage_update_admin_delegates() {
        let mut s = storage();
        let id = create_allowlist(&mut s);
        StorageCtx::enter(&mut s, |ctx| {
            let mut reg = PolicyRegistryStorage::new(ctx);
            crate::PolicyRegistry::stage_update_admin(&mut reg, id, NEW_ADMIN)
        })
        .unwrap();
        let pending = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).pending_policy_admin(id)
        })
        .unwrap();
        assert_eq!(pending, NEW_ADMIN);
    }

    #[test]
    fn trait_finalize_update_admin_delegates() {
        let mut s = storage();
        let id = create_allowlist(&mut s);
        StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).stage_update_admin(id, NEW_ADMIN)
        })
        .unwrap();
        s.set_caller(NEW_ADMIN);
        StorageCtx::enter(&mut s, |ctx| {
            let mut reg = PolicyRegistryStorage::new(ctx);
            crate::PolicyRegistry::finalize_update_admin(&mut reg, id)
        })
        .unwrap();
        let admin =
            StorageCtx::enter(&mut s, |ctx| PolicyRegistryStorage::new(ctx).get_policy_admin(id))
                .unwrap();
        assert_eq!(admin, NEW_ADMIN);
    }

    #[test]
    fn trait_renounce_admin_delegates() {
        let mut s = storage();
        let id = create_allowlist(&mut s);
        StorageCtx::enter(&mut s, |ctx| {
            let mut reg = PolicyRegistryStorage::new(ctx);
            crate::PolicyRegistry::renounce_admin(&mut reg, id)
        })
        .unwrap();
        let admin =
            StorageCtx::enter(&mut s, |ctx| PolicyRegistryStorage::new(ctx).get_policy_admin(id))
                .unwrap();
        assert_eq!(admin, Address::ZERO);
    }

    #[test]
    fn trait_update_allowlist_delegates() {
        let mut s = storage();
        let id = create_allowlist(&mut s);
        StorageCtx::enter(&mut s, |ctx| {
            let mut reg = PolicyRegistryStorage::new(ctx);
            crate::PolicyRegistry::update_allowlist(&mut reg, id, true, vec![ALICE])
        })
        .unwrap();
        assert!(is_authorized(&mut s, id, ALICE));
    }

    #[test]
    fn trait_update_blocklist_delegates() {
        let mut s = storage();
        let id = create_blocklist(&mut s);
        StorageCtx::enter(&mut s, |ctx| {
            let mut reg = PolicyRegistryStorage::new(ctx);
            crate::PolicyRegistry::update_blocklist(&mut reg, id, true, vec![ALICE])
        })
        .unwrap();
        assert!(!is_authorized(&mut s, id, ALICE));
    }

    #[test]
    fn trait_is_authorized_delegates() {
        let mut s = storage();
        let authorized = StorageCtx::enter(&mut s, |ctx| {
            let reg = PolicyRegistryStorage::new(ctx);
            crate::Policy::is_authorized(&reg, PolicyRegistryStorage::ALWAYS_ALLOW_ID, ALICE)
        })
        .unwrap();
        assert!(authorized);
    }

    #[test]
    fn trait_policy_exists_delegates() {
        let mut s = storage();
        let exists = StorageCtx::enter(&mut s, |ctx| {
            let reg = PolicyRegistryStorage::new(ctx);
            crate::Policy::policy_exists(&reg, PolicyRegistryStorage::ALWAYS_ALLOW_ID)
        })
        .unwrap();
        assert!(exists);
    }

    #[test]
    fn trait_get_policy_admin_delegates() {
        let mut s = storage();
        let admin = StorageCtx::enter(&mut s, |ctx| {
            let reg = PolicyRegistryStorage::new(ctx);
            crate::PolicyRegistry::get_policy_admin(&reg, PolicyRegistryStorage::ALWAYS_ALLOW_ID)
        })
        .unwrap();
        assert_eq!(admin, Address::ZERO);
    }

    #[test]
    fn trait_pending_policy_admin_delegates() {
        let mut s = storage();
        let id = create_allowlist(&mut s);
        let pending = StorageCtx::enter(&mut s, |ctx| {
            let reg = PolicyRegistryStorage::new(ctx);
            crate::PolicyRegistry::pending_policy_admin(&reg, id)
        })
        .unwrap();
        assert_eq!(pending, Address::ZERO);
    }
}
