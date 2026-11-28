use alloc::vec::Vec;

use alloy_primitives::{Address, LogData, U256, address, uint};

use crate::{PolicyAccounting, Result, StorageCtx, StorageLayout};

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

/// Storage of the `PolicyRegistry` precompile, rooted at the `base.policy_registry` ERC-7201
/// namespace.
///
/// Slots are append-only — never reorder across upgrades.
#[derive(Debug)]
pub struct PolicyRegistryStorage<'a> {
    /// Storage context of the current call.
    pub storage: StorageCtx<'a>,
}

impl<'a> PolicyRegistryStorage<'a> {
    /// Creates the policy registry's storage adapter.
    pub const fn new(storage: StorageCtx<'a>) -> Self {
        Self { storage }
    }
}

impl PolicyRegistryStorage<'_> {
    /// Singleton precompile address for the `PolicyRegistry`.
    pub const ADDRESS: Address = address!("8453000000000000000000000000000000000002");

    /// Built-in policy ID that always authorizes every account.
    ///
    /// A stable protocol sentinel consumed by B-20 tokens as their default policy; the
    /// encoding is owned by the frozen [`crate::PolicyRegistry`] logic and re-exported
    /// here for the registry's public API.
    pub const ALWAYS_ALLOW_ID: u64 = crate::PolicyRegistry::ALWAYS_ALLOW_ID;

    /// Built-in policy ID that always rejects every account.
    ///
    /// A stable protocol sentinel; encoding owned by [`crate::PolicyRegistry`].
    pub const ALWAYS_BLOCK_ID: u64 = crate::PolicyRegistry::ALWAYS_BLOCK_ID;

    /// ERC-7201 namespace id.
    pub const NAMESPACE: &'static str = "base.policy_registry";
    /// ERC-7201 namespace root.
    pub const ROOT: U256 =
        uint!(0x00503aeb06982fa1fe3151dc68f90b3946c55c449dfd447e49dcaece71ba4a00_U256);
    /// `mapping(uint64 policyId => uint256)`: [`PackedPolicy`] words.
    pub const POLICIES: U256 = StorageLayout::field(Self::ROOT, 0);
    /// `mapping(uint64 policyId => mapping(address account => bool)) members`.
    pub const MEMBERS: U256 = StorageLayout::field(Self::ROOT, 1);
    /// `mapping(uint64 policyId => address) pendingAdmins`.
    pub const PENDING_ADMINS: U256 = StorageLayout::field(Self::ROOT, 2);
    /// `uint64 nextCounter`, alone in its slot: the global monotonic counter for the low 56 bits
    /// of all custom policy IDs. Intentionally shared across ALLOWLIST and BLOCKLIST types — the
    /// type discriminator is encoded in the top byte, so both types draw from the same 56-bit
    /// space without collision.
    pub const NEXT_COUNTER: U256 = StorageLayout::field(Self::ROOT, 3);
    /// `mapping(uint64 policyId => uint64[]) children`: child policy IDs of composite
    /// (UNION/INTERSECT) policies, empty for simple policies. Introduced with the V2 logic.
    /// Full-set replacement on create/update; capped at 4 entries, which fit in a single
    /// dynamic-array element slot.
    pub const CHILDREN: U256 = StorageLayout::field(Self::ROOT, 4);

    /// Returns the slot of `mapping[policy_id]` for a mapping at `slot`.
    pub fn policy_slot(policy_id: u64, slot: U256) -> U256 {
        StorageLayout::mapping(&policy_id.to_be_bytes(), slot)
    }

    /// Returns the slot of `members[policy_id][account]`.
    pub fn member_slot(policy_id: u64, account: Address) -> U256 {
        StorageLayout::mapping(account.as_slice(), Self::policy_slot(policy_id, Self::MEMBERS))
    }
}

impl PolicyAccounting for PolicyRegistryStorage<'_> {
    fn caller(&self) -> Address {
        self.storage.caller()
    }

    fn read_policy_word(&self, policy_id: u64) -> Result<U256> {
        self.storage.read(Self::ADDRESS, Self::policy_slot(policy_id, Self::POLICIES))
    }

    fn write_policy_word(&mut self, policy_id: u64, word: U256) -> Result<()> {
        self.storage.write(Self::ADDRESS, Self::policy_slot(policy_id, Self::POLICIES), word)
    }

    fn read_member(&self, policy_id: u64, account: Address) -> Result<bool> {
        self.storage.read(Self::ADDRESS, Self::member_slot(policy_id, account))
    }

    fn set_member(&mut self, policy_id: u64, account: Address) -> Result<()> {
        self.storage.write(Self::ADDRESS, Self::member_slot(policy_id, account), true)
    }

    fn delete_member(&mut self, policy_id: u64, account: Address) -> Result<()> {
        self.storage.sstore(Self::ADDRESS, Self::member_slot(policy_id, account), U256::ZERO)
    }

    fn read_pending_admin(&self, policy_id: u64) -> Result<Address> {
        self.storage.read(Self::ADDRESS, Self::policy_slot(policy_id, Self::PENDING_ADMINS))
    }

    fn write_pending_admin(&mut self, policy_id: u64, admin: Address) -> Result<()> {
        let slot = Self::policy_slot(policy_id, Self::PENDING_ADMINS);
        self.storage.write(Self::ADDRESS, slot, admin)
    }

    fn delete_pending_admin(&mut self, policy_id: u64) -> Result<()> {
        let slot = Self::policy_slot(policy_id, Self::PENDING_ADMINS);
        self.storage.sstore(Self::ADDRESS, slot, U256::ZERO)
    }

    fn read_next_counter(&self) -> Result<u64> {
        self.storage.read(Self::ADDRESS, Self::NEXT_COUNTER)
    }

    fn write_next_counter(&mut self, counter: u64) -> Result<()> {
        self.storage.write(Self::ADDRESS, Self::NEXT_COUNTER, counter)
    }

    fn emit_event(&mut self, log: LogData) -> Result<()> {
        self.storage.emit_event(Self::ADDRESS, log)
    }

    fn mark_initialized(&mut self) -> Result<()> {
        self.storage.set_marker_code(Self::ADDRESS)
    }

    fn read_children(&self, policy_id: u64) -> Result<Vec<u64>> {
        self.storage.read_u64_array(Self::ADDRESS, Self::policy_slot(policy_id, Self::CHILDREN))
    }

    fn write_children(&mut self, policy_id: u64, child_policy_ids: &[u64]) -> Result<()> {
        let slot = Self::policy_slot(policy_id, Self::CHILDREN);
        self.storage.write_u64_array(Self::ADDRESS, slot, child_policy_ids)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes, U256, address, keccak256, uint};

    use crate::{
        BasePrecompileError, HashMapStorageProvider, IPolicyRegistry::PolicyType,
        PolicyAccounting, PolicyRegistry, PolicyVersion, PrecompileStorageProvider, StorageCtx,
        StorageLayout, policy::storage::{PackedPolicy, PolicyRegistryStorage},
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

    /// `keccak256(abi.encode(uint64 key, uint256 slot))`, Solidity's mapping slot.
    fn solidity_mapping_slot(key: u64, slot: U256) -> U256 {
        let encoded = [U256::from(key).to_be_bytes::<32>(), slot.to_be_bytes::<32>()].concat();
        U256::from_be_bytes(keccak256(encoded).0)
    }

    /// Seeds both built-in policies via the pinned V1 bootstrap and returns the provider.
    fn seeded_storage() -> HashMapStorageProvider {
        let mut s = HashMapStorageProvider::new(1);
        s.set_caller(ADMIN);
        StorageCtx::enter(&mut s, |ctx| {
            let mut storage = PolicyRegistryStorage::new(ctx);
            PolicyRegistry::new(PolicyVersion::V1).ensure_initialized_and_get_counter(&mut storage)
        })
        .unwrap();
        s
    }

    /// Creates an ALLOWLIST policy through the pinned V1 logic.
    fn create_allowlist(s: &mut HashMapStorageProvider) -> u64 {
        StorageCtx::enter(s, |ctx| {
            let mut storage = PolicyRegistryStorage::new(ctx);
            PolicyRegistry::new(PolicyVersion::V1).create_policy(&mut storage, ADMIN, PolicyType::ALLOWLIST)
        })
        .unwrap()
    }

    #[test]
    fn policy_registry_namespace_matches_base_std_root() {
        assert_eq!(
            StorageLayout::erc7201_root(PolicyRegistryStorage::NAMESPACE),
            POLICY_REGISTRY_ROOT
        );
        assert_eq!(PolicyRegistryStorage::ROOT, POLICY_REGISTRY_ROOT);
        assert_eq!(PolicyRegistryStorage::POLICIES, POLICY_REGISTRY_ROOT);
        assert_eq!(PolicyRegistryStorage::MEMBERS, POLICY_REGISTRY_ROOT + U256::from(1));
        assert_eq!(PolicyRegistryStorage::PENDING_ADMINS, POLICY_REGISTRY_ROOT + U256::from(2));
        assert_eq!(PolicyRegistryStorage::NEXT_COUNTER, POLICY_REGISTRY_ROOT + U256::from(3));
        assert_eq!(PolicyRegistryStorage::CHILDREN, POLICY_REGISTRY_ROOT + U256::from(4));
    }

    #[test]
    fn children_roundtrip_and_match_base_std_layout() {
        let mut s = seeded_storage();
        let children = alloc::vec![7u64, 9u64, 11u64];
        StorageCtx::enter(&mut s, |ctx| {
            let mut storage = PolicyRegistryStorage::new(ctx);
            storage.write_children(2, &children).unwrap();
            assert_eq!(storage.read_children(2).unwrap(), children);
        });
        StorageCtx::enter(&mut s, |ctx| {
            // base-std `childrenSlot(id)` = keccak256(abi.encode(id, childrenBaseSlot)) holds the
            // dynamic-array length; elements pack 4x uint64 per slot at keccak256(that), low first.
            let len_slot = solidity_mapping_slot(2, PolicyRegistryStorage::CHILDREN);
            assert_eq!(ctx.sload(PolicyRegistryStorage::ADDRESS, len_slot).unwrap(), U256::from(3));
            let data_slot = U256::from_be_bytes(keccak256(len_slot.to_be_bytes::<32>()).0);
            let packed = ctx.sload(PolicyRegistryStorage::ADDRESS, data_slot).unwrap();
            let expected = U256::from(7u64) | (U256::from(9u64) << 64) | (U256::from(11u64) << 128);
            assert_eq!(packed, expected, "uint64[] elements must pack low-element-first");
        });
    }

    #[test]
    fn policy_registry_writes_use_base_std_namespace_slots() {
        let mut s = seeded_storage();
        let id = create_allowlist(&mut s);

        StorageCtx::enter(&mut s, |ctx| {
            let word = |slot| ctx.sload(PolicyRegistryStorage::ADDRESS, slot).unwrap();
            let policy_slot = solidity_mapping_slot(id, PolicyRegistryStorage::POLICIES);
            assert_ne!(word(policy_slot), U256::ZERO);
            assert_eq!(word(PolicyRegistryStorage::NEXT_COUNTER), U256::from(3));
            assert_eq!(word(solidity_mapping_slot(id, U256::ZERO)), U256::ZERO);
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
            Bytes::from_static(&[0xef]),
        )
        .unwrap();

        let id = create_allowlist(&mut s);
        assert_eq!(id & PolicyRegistry::COUNTER_MASK, 2);
        StorageCtx::enter(&mut s, |ctx| {
            let storage = PolicyRegistryStorage::new(ctx);
            assert!(
                PolicyRegistry::new(PolicyVersion::V1)
                    .policy_exists(&storage, PolicyRegistry::ALWAYS_ALLOW_ID)
                    .unwrap()
            );
            assert!(
                PolicyRegistry::new(PolicyVersion::V1)
                    .policy_exists(&storage, PolicyRegistry::ALWAYS_BLOCK_ID)
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
            PolicyRegistry::new(PolicyVersion::V1).create_policy(&mut storage, ADMIN, PolicyType::ALLOWLIST)
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
            PolicyRegistry::new(PolicyVersion::V1).update_allowlist(&mut storage, id, true, alloc::vec![ALICE])
        })
        .unwrap_err();
        assert_eq!(err, BasePrecompileError::StaticCallViolation);
    }
}
