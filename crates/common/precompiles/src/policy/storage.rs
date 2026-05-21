use alloc::vec::Vec;

use alloy_primitives::{Address, U256, address};
use base_precompile_macros::contract;
use base_precompile_storage::{BasePrecompileError, ContractStorage, Handler, Mapping, Result};

use super::{IPolicyRegistry, IPolicyRegistry::PolicyType};

/// A packed policy storage word.
///
/// Layout: `[255:168]` reserved (zero) | `[167:8]` admin (160 bits) | `[7:0]` `PolicyType`.
///
/// The inner value is always non-zero for valid custom policies because ALLOWLIST = 2 and
/// BLOCKLIST = 3 are both non-zero. This means the zero value reliably signals "never created",
/// even after `renounce_admin` sets admin to `Address::ZERO` (the type byte is preserved).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PackedPolicy(U256);

impl PackedPolicy {
    /// Packs `admin` and `policy_type` into a storage word.
    /// Accepts `PolicyType` to prevent invalid discriminants at construction time.
    pub(crate) fn new(admin: Address, policy_type: PolicyType) -> Self {
        Self::from_parts(admin, policy_type as u8)
    }

    /// Returns a new word with the same type byte but a different admin.
    /// Used when transferring or renouncing admin without changing the policy type.
    pub(crate) fn with_admin(self, new_admin: Address) -> Self {
        Self::from_parts(new_admin, self.policy_type_u8())
    }

    /// Returns the admin address stored in `[167:8]`.
    pub(crate) fn admin(self) -> Address {
        let bytes = (self.0 >> 8usize).to_be_bytes::<32>();
        Address::from_slice(&bytes[12..])
    }

    /// Returns the raw `PolicyType` discriminant stored in `[7:0]`.
    pub(crate) const fn policy_type_u8(self) -> u8 {
        self.0.to_be_bytes::<32>()[31]
    }

    /// Returns `true` if the word is zero (policy was never created).
    pub(crate) fn is_zero(self) -> bool {
        self.0.is_zero()
    }

    /// Returns the raw `U256` value for writing to storage.
    pub(crate) const fn into_u256(self) -> U256 {
        self.0
    }

    /// Wraps a raw storage word without validating the type discriminant.
    /// Intended only for reading words back from storage.
    pub(crate) const fn from_raw(v: U256) -> Self {
        Self(v)
    }

    fn from_parts(admin: Address, policy_type_u8: u8) -> Self {
        let mut word = [0u8; 32];
        word[12..32].copy_from_slice(admin.as_slice());
        Self((U256::from_be_slice(&word) << 8) | U256::from(policy_type_u8))
    }
}

/// Storage layout for the `PolicyRegistry` precompile.
///
/// Slots are append-only — never reorder across hardforks.
#[contract(addr = Self::ADDRESS)]
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
    pub const ALWAYS_ALLOW_ID: u64 = 0;
    /// Built-in policy ID that always rejects every account.
    pub const ALWAYS_BLOCK_ID: u64 = 1;

    const ALLOWLIST_TYPE: u8 = PolicyType::ALLOWLIST as u8;
    const BLOCKLIST_TYPE: u8 = PolicyType::BLOCKLIST as u8;
    const COUNTER_MASK: u64 = (1u64 << 56) - 1;
    const INITIAL_CUSTOM_COUNTER: u64 = 2;
    const POLICY_ID_TYPE_SHIFT: usize = 56;

    fn require_write(&self) -> Result<()> {
        if self.storage.is_static() {
            return Err(BasePrecompileError::StaticCallViolation);
        }
        Ok(())
    }

    const fn policy_id_type(policy_id: u64) -> u8 {
        (policy_id >> Self::POLICY_ID_TYPE_SHIFT) as u8
    }

    fn require_well_formed(policy_id: u64) -> Result<()> {
        if Self::policy_id_type(policy_id) > PolicyType::BLOCKLIST as u8 {
            return Err(BasePrecompileError::revert(IPolicyRegistry::MalformedPolicyId {
                policyId: policy_id,
            }));
        }
        Ok(())
    }

    fn require_custom(&self, policy_id: u64) -> Result<PackedPolicy> {
        Self::require_well_formed(policy_id)?;
        let packed = PackedPolicy::from_raw(self.policies.at(&policy_id).read()?);
        if packed.is_zero() {
            return Err(BasePrecompileError::revert(IPolicyRegistry::PolicyNotFound {}));
        }
        Ok(packed)
    }

    fn next_counter(&self) -> Result<u64> {
        let counter = self.next_counter.read()?;
        Ok(counter.max(Self::INITIAL_CUSTOM_COUNTER))
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

    /// Creates a new ALLOWLIST or BLOCKLIST policy, returning its encoded ID.
    pub fn create_policy(&mut self, admin: Address, policy_type: PolicyType) -> Result<u64> {
        self.require_write()?;
        let policy_type_u8 = policy_type.as_discriminant()?;
        if admin == Address::ZERO {
            return Err(BasePrecompileError::revert(IPolicyRegistry::ZeroAddress {}));
        }

        // The registry account must be non-empty before the first policy storage write; otherwise
        // the EVM path can prune writes made under an empty native-precompile account.
        // TODO: Revisit this guard against the finalized Beryl gas model, since `is_initialized`
        // charges warm/cold account-read gas before skipping repeated `set_code`.
        if !self.is_initialized()? {
            self.__initialize()?;
        }

        let counter = self.next_counter()?;
        let next = counter.checked_add(1).ok_or_else(BasePrecompileError::under_overflow)?;
        self.next_counter.write(next)?;
        let policy_id = Self::make_id(policy_type_u8, counter);
        let packed = PackedPolicy::new(admin, policy_type).into_u256();
        self.policies.at_mut(&policy_id).write(packed)?;

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
        let policy_type_u8 = policy_type.as_discriminant()?;
        let caller = self.storage.caller();
        for account in &accounts {
            self.members.at_mut(&policy_id).at_mut(account).write(true)?;
        }
        match policy_type_u8 {
            Self::ALLOWLIST_TYPE => self.emit_event(IPolicyRegistry::AllowlistUpdated {
                policyId: policy_id,
                updater: caller,
                allowed: true,
                accounts,
            })?,
            Self::BLOCKLIST_TYPE => self.emit_event(IPolicyRegistry::BlocklistUpdated {
                policyId: policy_id,
                updater: caller,
                blocked: true,
                accounts,
            })?,
            _ => unreachable!("policy_type validated by create_policy"),
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
            previousAdmin: caller,
            newAdmin: new_admin,
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
        let (packed, caller) = self.require_admin(policy_id)?;
        if packed.policy_type_u8() != expected_type {
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
    pub fn is_authorized(&self, policy_id: u64, account: Address) -> Result<bool> {
        Self::require_well_formed(policy_id)?;
        if policy_id == Self::ALWAYS_ALLOW_ID {
            return Ok(true);
        }
        if policy_id == Self::ALWAYS_BLOCK_ID {
            return Ok(false);
        }
        let packed = PackedPolicy::from_raw(self.policies.at(&policy_id).read()?);
        if packed.is_zero() {
            return Err(BasePrecompileError::revert(IPolicyRegistry::PolicyNotFound {}));
        }
        let member = self.members.at(&policy_id).at(&account).read()?;
        match packed.policy_type_u8() {
            Self::ALLOWLIST_TYPE => Ok(member),
            Self::BLOCKLIST_TYPE => Ok(!member),
            _ => Err(BasePrecompileError::enum_conversion_error()),
        }
    }

    /// Returns `true` if `policy_id` refers to an existing or built-in policy.
    pub fn policy_exists(&self, policy_id: u64) -> Result<bool> {
        Self::require_well_formed(policy_id)?;
        if policy_id == Self::ALWAYS_ALLOW_ID || policy_id == Self::ALWAYS_BLOCK_ID {
            return Ok(true);
        }
        let packed = PackedPolicy::from_raw(self.policies.at(&policy_id).read()?);
        Ok(!packed.is_zero())
    }

    /// Returns the `PolicyType` of `policy_id`, including built-in IDs.
    pub fn get_policy_type(&self, policy_id: u64) -> Result<PolicyType> {
        Self::require_well_formed(policy_id)?;
        if policy_id == Self::ALWAYS_ALLOW_ID {
            return Ok(PolicyType::ALWAYS_ALLOW);
        }
        if policy_id == Self::ALWAYS_BLOCK_ID {
            return Ok(PolicyType::ALWAYS_BLOCK);
        }
        let packed = PackedPolicy::from_raw(self.policies.at(&policy_id).read()?);
        if packed.is_zero() {
            return Err(BasePrecompileError::revert(IPolicyRegistry::PolicyNotFound {}));
        }
        PolicyType::try_from(packed.policy_type_u8())
            .map_err(|_| BasePrecompileError::enum_conversion_error())
    }

    /// Returns the current admin of `policy_id`, or `address(0)` for built-in policies.
    pub fn get_policy_admin(&self, policy_id: u64) -> Result<Address> {
        Self::require_well_formed(policy_id)?;
        if policy_id == Self::ALWAYS_ALLOW_ID || policy_id == Self::ALWAYS_BLOCK_ID {
            return Ok(Address::ZERO);
        }
        let packed = PackedPolicy::from_raw(self.policies.at(&policy_id).read()?);
        if packed.is_zero() {
            return Err(BasePrecompileError::revert(IPolicyRegistry::PolicyNotFound {}));
        }
        Ok(packed.admin())
    }

    /// Returns the pending admin staged for `policy_id`, or `address(0)` if none.
    pub fn pending_policy_admin(&self, policy_id: u64) -> Result<Address> {
        Self::require_well_formed(policy_id)?;
        if policy_id == Self::ALWAYS_ALLOW_ID || policy_id == Self::ALWAYS_BLOCK_ID {
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

    fn get_policy_type(&self, policy_id: u64) -> Result<PolicyType> {
        PolicyRegistryStorage::get_policy_type(self, policy_id)
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
    use alloy_primitives::{Address, U256, address};
    use alloy_sol_types::SolEvent;
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use super::*;
    use crate::IPolicyRegistry;

    // --- PackedPolicy unit tests ---

    #[test]
    fn packed_policy_new_roundtrips_admin_and_type() {
        let p = PackedPolicy::new(ADMIN, PolicyType::ALLOWLIST);
        assert_eq!(p.admin(), ADMIN);
        assert_eq!(p.policy_type_u8(), PolicyType::ALLOWLIST as u8);
        assert!(!p.is_zero());
    }

    #[test]
    fn packed_policy_zero_signals_never_created() {
        let p = PackedPolicy::from_raw(U256::ZERO);
        assert!(p.is_zero());
    }

    #[test]
    fn packed_policy_renounced_admin_is_non_zero() {
        let p = PackedPolicy::new(Address::ZERO, PolicyType::ALLOWLIST);
        assert!(!p.is_zero());
        assert_eq!(p.admin(), Address::ZERO);
        assert_eq!(p.policy_type_u8(), PolicyType::ALLOWLIST as u8);
    }

    #[test]
    fn packed_policy_into_u256_from_raw_roundtrip() {
        let p = PackedPolicy::new(ADMIN, PolicyType::BLOCKLIST);
        let p2 = PackedPolicy::from_raw(p.into_u256());
        assert_eq!(p, p2);
        assert_eq!(p2.admin(), ADMIN);
        assert_eq!(p2.policy_type_u8(), PolicyType::BLOCKLIST as u8);
    }

    #[test]
    fn packed_policy_different_admins_produce_different_words() {
        let other = address!("0x2000000000000000000000000000000000000002");
        assert_ne!(
            PackedPolicy::new(ADMIN, PolicyType::ALLOWLIST),
            PackedPolicy::new(other, PolicyType::ALLOWLIST)
        );
    }

    const ADMIN: Address = address!("0x1000000000000000000000000000000000000001");
    const ALICE: Address = address!("0xA000000000000000000000000000000000000001");
    const BOB: Address = address!("0xB000000000000000000000000000000000000001");
    const NEW_ADMIN: Address = address!("0x2000000000000000000000000000000000000002");

    fn storage() -> HashMapStorageProvider {
        let mut s = HashMapStorageProvider::new(1);
        s.set_caller(ADMIN);
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
    fn unknown_policy_id_returns_policy_not_found() {
        let mut s = storage();
        let err = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).is_authorized(0xdeadbeef, ALICE)
        })
        .unwrap_err();
        assert!(matches!(err, BasePrecompileError::Revert(_)));
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
    fn create_policy_invalid_type_reverts() {
        let mut s = storage();
        let err = StorageCtx::enter(&mut s, |ctx| {
            PolicyRegistryStorage::new(ctx).create_policy(ADMIN, PolicyType::ALWAYS_ALLOW)
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
        let mut s = storage();
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

    // --- get_policy_type for built-in IDs ---

    #[test]
    fn get_policy_type_builtin_ids() {
        let mut s = storage();
        assert_eq!(
            StorageCtx::enter(&mut s, |ctx| {
                PolicyRegistryStorage::new(ctx)
                    .get_policy_type(PolicyRegistryStorage::ALWAYS_ALLOW_ID)
            })
            .unwrap(),
            PolicyType::ALWAYS_ALLOW
        );
        assert_eq!(
            StorageCtx::enter(&mut s, |ctx| {
                PolicyRegistryStorage::new(ctx)
                    .get_policy_type(PolicyRegistryStorage::ALWAYS_BLOCK_ID)
            })
            .unwrap(),
            PolicyType::ALWAYS_BLOCK
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
    fn trait_get_policy_type_delegates() {
        let mut s = storage();
        let pt = StorageCtx::enter(&mut s, |ctx| {
            let reg = PolicyRegistryStorage::new(ctx);
            crate::PolicyRegistry::get_policy_type(&reg, PolicyRegistryStorage::ALWAYS_ALLOW_ID)
        })
        .unwrap();
        assert_eq!(pt, PolicyType::ALWAYS_ALLOW);
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
