use alloy_primitives::{Address, LogData, U256, address};
use base_precompile_macros::contract;
use base_precompile_storage::{Handler, Mapping, Result};

use crate::PolicyAccounting;

/// A packed policy storage word.
///
/// Layout: `[255]` exists flag | `[254:160]` reserved (zero) | `[159:0]` admin (160 bits).
///
/// The policy type is not stored here — it is encoded in the high byte of the policy ID
/// and derived from there. Bit 255 is always set for any written slot, making the zero word
/// a reliable "never written" sentinel even when admin is `Address::ZERO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackedPolicy(U256);

impl PackedPolicy {
    /// Bit 255: the highest bit of limb 3.
    const EXISTS_BIT: U256 = U256::from_limbs([0, 0, 0, 1u64 << 63]);
    /// Mask covering the low 160 bits where the admin address lives.
    const ADMIN_MASK: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0xFFFF_FFFF, 0]);

    /// Creates a packed policy word for `admin`.
    pub fn new(admin: Address) -> Self {
        let mut word = [0u8; 32];
        word[12..32].copy_from_slice(admin.as_slice());
        Self(U256::from_be_slice(&word) | Self::EXISTS_BIT)
    }

    /// Returns this policy word with its admin replaced.
    pub fn with_admin(self, new_admin: Address) -> Self {
        Self::new(new_admin)
    }

    /// Returns the admin address encoded in this policy word.
    pub fn admin(self) -> Address {
        let bytes = (self.0 & Self::ADMIN_MASK).to_be_bytes::<32>();
        Address::from_slice(&bytes[12..])
    }

    /// Returns whether this policy word has the exists bit set.
    pub fn exists(self) -> bool {
        !(self.0 & Self::EXISTS_BIT).is_zero()
    }

    /// Returns the raw packed policy word.
    pub const fn into_u256(self) -> U256 {
        self.0
    }

    /// Creates a packed policy wrapper from a raw storage word.
    pub const fn from_raw(v: U256) -> Self {
        Self(v)
    }
}

/// Storage layout for the `PolicyRegistry` precompile.
///
/// Slots are append-only — never reorder across upgrades.
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
    pub const ADDRESS: Address = address!("8453000000000000000000000000000000000002");

    /// Built-in policy ID that always authorizes every account.
    ///
    /// A stable protocol sentinel consumed by B-20 tokens as their default policy; the
    /// encoding is owned by the frozen [`crate::PolicyRegistryV1`] logic and re-exported
    /// here for the registry's public API.
    pub const ALWAYS_ALLOW_ID: u64 = crate::PolicyRegistryV1::ALWAYS_ALLOW_ID;

    /// Built-in policy ID that always rejects every account.
    ///
    /// A stable protocol sentinel; encoding owned by [`crate::PolicyRegistryV1`].
    pub const ALWAYS_BLOCK_ID: u64 = crate::PolicyRegistryV1::ALWAYS_BLOCK_ID;
}

impl PolicyAccounting for PolicyRegistryStorage<'_> {
    fn registry_address(&self) -> Address {
        Self::ADDRESS
    }

    fn caller(&self) -> Address {
        self.storage.caller()
    }

    fn read_policy_word(&self, policy_id: u64) -> Result<U256> {
        self.policies.at(&policy_id).read()
    }

    fn write_policy_word(&mut self, policy_id: u64, word: U256) -> Result<()> {
        self.policies.at_mut(&policy_id).write(word)
    }

    fn read_member(&self, policy_id: u64, account: Address) -> Result<bool> {
        self.members.at(&policy_id).at(&account).read()
    }

    fn set_member(&mut self, policy_id: u64, account: Address) -> Result<()> {
        self.members.at_mut(&policy_id).at_mut(&account).write(true)
    }

    fn delete_member(&mut self, policy_id: u64, account: Address) -> Result<()> {
        self.members.at_mut(&policy_id).at_mut(&account).delete()
    }

    fn read_pending_admin(&self, policy_id: u64) -> Result<Address> {
        self.pending_admins.at(&policy_id).read()
    }

    fn write_pending_admin(&mut self, policy_id: u64, admin: Address) -> Result<()> {
        self.pending_admins.at_mut(&policy_id).write(admin)
    }

    fn delete_pending_admin(&mut self, policy_id: u64) -> Result<()> {
        self.pending_admins.at_mut(&policy_id).delete()
    }

    fn read_next_counter(&self) -> Result<u64> {
        self.next_counter.read()
    }

    fn write_next_counter(&mut self, counter: u64) -> Result<()> {
        self.next_counter.write(counter)
    }

    fn emit_event(&mut self, log: LogData) -> Result<()> {
        PolicyRegistryStorage::emit_event(self, log)
    }

    fn mark_initialized(&mut self) -> Result<()> {
        self.__initialize()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes, U256, address, uint};
    use base_precompile_storage::{
        BasePrecompileError, HashMapStorageProvider, PrecompileStorageProvider, StorageCtx,
        StorageKey,
    };
    use revm::state::Bytecode;

    use crate::{
        IPolicyRegistry::PolicyType,
        PolicyRegistryLogic, PolicyRegistryV1,
        policy::storage::{PackedPolicy, PolicyRegistryStorage, slots},
    };

    const ADMIN: Address = address!("0x1000000000000000000000000000000000000001");
    const ALICE: Address = address!("0xA000000000000000000000000000000000000001");
    const POLICY_REGISTRY_ROOT: U256 =
        uint!(0x00503aeb06982fa1fe3151dc68f90b3946c55c449dfd447e49dcaece71ba4a00_U256);

    // --- PackedPolicy value-type unit tests ---

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
        assert_eq!(p.admin(), Address::ZERO, "zero word admin must be Address::ZERO");
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

    #[test]
    fn packed_policy_new_roundtrips_admin_for_various_addresses() {
        let addrs = [
            ADMIN,
            Address::ZERO,
            address!("0xffffffffffffffffffffffffffffffffffffffff"),
            address!("0x2000000000000000000000000000000000000002"),
        ];
        for addr in addrs {
            let p = PackedPolicy::new(addr);
            assert_eq!(p.admin(), addr, "admin must round-trip for address {addr}");
            assert!(p.exists(), "exists must be true for any new PackedPolicy");
        }
    }

    #[test]
    fn exists_bit_does_not_bleed_into_admin_bits() {
        // The EXISTS_BIT is at bit 255; the admin is extracted from bits [159:0].
        let p = PackedPolicy::new(ADMIN);
        assert_eq!(p.admin(), ADMIN, "exists bit must not corrupt the admin field");
        let exists_only = PackedPolicy::from_raw(PackedPolicy::EXISTS_BIT);
        assert_eq!(exists_only.admin(), Address::ZERO, "exists-only word must have zero admin");
        assert!(exists_only.exists());
    }

    // --- Storage layout / namespace (consensus-critical slot mapping) ---

    /// Seeds both built-in policies via the pinned V1 bootstrap and returns the provider.
    fn seeded_storage() -> HashMapStorageProvider {
        let mut s = HashMapStorageProvider::new(1);
        s.set_caller(ADMIN);
        StorageCtx::enter(&mut s, |ctx| {
            let mut storage = PolicyRegistryStorage::new(ctx);
            PolicyRegistryV1.ensure_initialized_and_get_counter(&mut storage)
        })
        .unwrap();
        s
    }

    /// Creates an ALLOWLIST policy through the pinned V1 logic.
    fn create_allowlist(s: &mut HashMapStorageProvider) -> u64 {
        StorageCtx::enter(s, |ctx| {
            let mut storage = PolicyRegistryStorage::new(ctx);
            PolicyRegistryV1.create_policy(&mut storage, ADMIN, PolicyType::ALLOWLIST)
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
        let mut s = seeded_storage();
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

    #[test]
    fn seeds_builtins_when_bytecode_is_pre_warmed() {
        // Harnesses (anvil/foundry forks) may pre-warm precompile bytecode to satisfy Solidity's
        // EXTCODESIZE check before any policy is created. The gate keys on next_counter, not on
        // bytecode presence, so seeding still runs and the counter still lands at 2.
        let mut s = HashMapStorageProvider::new(1);
        s.set_caller(ADMIN);
        PrecompileStorageProvider::set_code(
            &mut s,
            PolicyRegistryStorage::ADDRESS,
            Bytecode::new_legacy(Bytes::from_static(&[0xef])),
        )
        .unwrap();

        let id = create_allowlist(&mut s);
        assert_eq!(id & PolicyRegistryV1::COUNTER_MASK, 2);
        StorageCtx::enter(&mut s, |ctx| {
            let storage = PolicyRegistryStorage::new(ctx);
            assert!(
                PolicyRegistryV1
                    .policy_exists(&storage, PolicyRegistryV1::ALWAYS_ALLOW_ID)
                    .unwrap()
            );
            assert!(
                PolicyRegistryV1
                    .policy_exists(&storage, PolicyRegistryV1::ALWAYS_BLOCK_ID)
                    .unwrap()
            );
        });
    }

    // --- static-call guard (storage-layer write protection) ---

    #[test]
    fn write_in_static_context_reverts() {
        let mut s = seeded_storage();
        s.set_static(true);
        let err = StorageCtx::enter(&mut s, |ctx| {
            let mut storage = PolicyRegistryStorage::new(ctx);
            PolicyRegistryV1.create_policy(&mut storage, ADMIN, PolicyType::ALLOWLIST)
        })
        .unwrap_err();
        assert_eq!(err, BasePrecompileError::StaticCallViolation);
    }

    #[test]
    fn update_allowlist_static_call_reverts() {
        let mut s = seeded_storage();
        let id = create_allowlist(&mut s);
        s.set_static(true);
        let err = StorageCtx::enter(&mut s, |ctx| {
            let mut storage = PolicyRegistryStorage::new(ctx);
            PolicyRegistryV1.update_allowlist(&mut storage, id, true, alloc::vec![ALICE])
        })
        .unwrap_err();
        assert_eq!(err, BasePrecompileError::StaticCallViolation);
    }
}
