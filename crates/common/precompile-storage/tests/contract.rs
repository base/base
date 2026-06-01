//! End-to-end test: exercises the `#[contract]` macro with `HashMapStorageProvider`.
//!
//! Validates that the macro generates correct storage layout,
//! typed getter/setter fields work round-trip, and collision detection fires.
use alloy_primitives::{Address, U256, address, keccak256};
use base_precompile_macros::contract;
use base_precompile_storage::{Handler, Mapping, StorageCtx, StorageKey, setup_storage};

const TEST_ADDR: Address = address!("0000000000000000000000000000000000001234");

fn data_slot(slot: U256) -> U256 {
    U256::from_be_bytes(keccak256(slot.to_be_bytes::<32>()).0)
}

fn erc7201_root(id: &str) -> U256 {
    let id_hash = U256::from_be_bytes(keccak256(id.as_bytes()).0);
    let shifted = id_hash.checked_sub(U256::ONE).unwrap();
    let root = U256::from_be_bytes(keccak256(shifted.to_be_bytes::<32>()).0);
    root & (U256::MAX - U256::from(0xffu64))
}

fn word_from_chunk(data: &[u8], chunk_index: usize) -> U256 {
    let mut word = [0u8; 32];
    let start = chunk_index * 32;
    let end = (start + 32).min(data.len());
    word[..end - start].copy_from_slice(&data[start..end]);
    U256::from_be_bytes(word)
}

/// A minimal token storage layout for integration testing.
#[contract(addr = TEST_ADDR)]
pub struct TestToken {
    pub owner: Address,
    pub total_supply: U256,
    pub balances: Mapping<Address, U256>,
    pub allowances: Mapping<Address, Mapping<Address, U256>>,
}

#[test]
fn test_contract_macro_basic_roundtrip() {
    let (mut storage, _) = setup_storage();

    StorageCtx::enter(&mut storage, |ctx| {
        let mut token = TestToken::new(ctx);

        let alice = Address::from([0xaa; 20]);
        let bob = Address::from([0xbb; 20]);

        // Write owner and total_supply
        token.owner.write(alice).unwrap();
        token.total_supply.write(U256::from(1_000_000u64)).unwrap();

        // Read back
        assert_eq!(token.owner.read().unwrap(), alice);
        assert_eq!(token.total_supply.read().unwrap(), U256::from(1_000_000u64));

        // Write and read a mapping entry
        token.balances.at_mut(&alice).write(U256::from(500u64)).unwrap();
        assert_eq!(token.balances.at(&alice).read().unwrap(), U256::from(500u64));
        assert_eq!(token.balances.at(&bob).read().unwrap(), U256::ZERO);

        // Nested mapping
        token.allowances[alice][bob].write(U256::from(100u64)).unwrap();
        assert_eq!(token.allowances[alice][bob].read().unwrap(), U256::from(100u64));
        assert_eq!(token.allowances[bob][alice].read().unwrap(), U256::ZERO);
    });
}

#[test]
fn test_contract_slots_are_deterministic() {
    // Verify that the generated slot constants are stable across runs.
    // owner is field 0 → slot 0, total_supply is field 1 → slot 1.
    assert_eq!(slots::OWNER, U256::ZERO);
    assert_eq!(slots::TOTAL_SUPPLY, U256::from(1u64));
    assert_eq!(slots::BALANCES, U256::from(2u64));
    assert_eq!(slots::ALLOWANCES, U256::from(3u64));
}

#[test]
fn test_contract_mapping_slot_derivation() {
    // Verify that mapping slots match the Solidity keccak256 derivation.
    let alice = Address::from([0xaa; 20]);
    let expected = alice.mapping_slot(slots::BALANCES);

    let (mut storage, _) = setup_storage();
    StorageCtx::enter(&mut storage, |ctx| {
        let mut token = TestToken::new(ctx);
        let write_value = U256::from(42u64);
        token.balances.at_mut(&alice).write(write_value).unwrap();

        // Verify the raw storage slot matches the expected derivation.
        let raw = ctx.sload(TEST_ADDR, expected).unwrap();
        assert_eq!(raw, write_value);
    });
}

#[test]
fn test_contract_multiple_instances_independent() {
    let (mut storage1, _) = setup_storage();
    let (mut storage2, _) = setup_storage();

    let alice = Address::from([0xaa; 20]);

    StorageCtx::enter(&mut storage1, |ctx| {
        let mut t1 = TestToken::new(ctx);
        t1.balances.at_mut(&alice).write(U256::from(100u64)).unwrap();
    });

    StorageCtx::enter(&mut storage2, |ctx| {
        let t2 = TestToken::new(ctx);
        // storage2 is independent — balance should be zero.
        assert_eq!(t2.balances.at(&alice).read().unwrap(), U256::ZERO);
    });
}

mod namespaced_layout {
    use alloy_primitives::{Address, U256, address, uint};
    use base_precompile_macros::{Storable, contract};
    use base_precompile_storage::{Handler, Mapping, StorageCtx, StorageKey, setup_storage};

    use super::{data_slot, word_from_chunk};

    const NAMESPACED_ADDR: Address = address!("0000000000000000000000000000000000004321");
    const EXPECTED_ROOT: U256 =
        uint!(0x50861ae81a7f4392b927efbaeecf8f091f3bd39245aa45ea91499a137b8b3100_U256);

    /// A storage section embedded into the token storage layout.
    #[derive(Debug, Clone, Storable)]
    struct PolicyNamespace {
        label: String,
        balances: Mapping<Address, U256>,
        checkpoints: [U256; 3],
        packed_flags: [u16; 20],
        amounts: Vec<U256>,
    }

    /// Token storage with an embedded policy section rooted at the ERC-7201 namespace.
    #[contract(addr = NAMESPACED_ADDR)]
    pub struct NamespacedStorage {
        pub admin: Address,
        #[namespace("b20.policy")]
        pub policy: PolicyNamespace,
        pub total_supply: U256,
        #[namespace("b20.policy")]
        pub policy_owner: Address,
    }

    #[test]
    fn namespace_root_and_offsets_are_deterministic() {
        assert_eq!(slots::ADMIN, U256::ZERO);
        assert_eq!(slots::POLICY, EXPECTED_ROOT);
        assert_eq!(slots::TOTAL_SUPPLY, U256::ONE);
        assert_eq!(
            slots::POLICY_OWNER,
            EXPECTED_ROOT + U256::from(__packing_policy_namespace::SLOT_COUNT)
        );
    }

    #[test]
    fn namespaced_struct_field_handles_dynamic_mapping_and_array_storage() {
        let (mut storage, _) = setup_storage();
        let owner = Address::from([0xaa; 20]);
        let policy_owner = Address::from([0xcc; 20]);
        let long_label =
            "namespaced-string-storage-value-that-spans-more-than-one-word-for-layout".to_owned();
        assert!(long_label.len() > 64);
        let amounts = vec![U256::from(11), U256::from(22), U256::from(33)];
        let policy_value = PolicyNamespace {
            label: long_label.clone(),
            balances: Mapping::default(),
            checkpoints: [U256::from(1), U256::from(2), U256::from(3)],
            packed_flags: [0; 20],
            amounts: amounts.clone(),
        };
        let _ = (
            &policy_value.label,
            &policy_value.balances,
            &policy_value.checkpoints,
            &policy_value.packed_flags,
            &policy_value.amounts,
        );

        StorageCtx::enter(&mut storage, |ctx| {
            let mut layout = NamespacedStorage::new(ctx);
            layout.admin.write(owner).unwrap();
            layout.policy.label.write(long_label.clone()).unwrap();
            layout.policy.balances.at_mut(&owner).write(U256::from(500)).unwrap();
            layout.policy.checkpoints.write([U256::from(1), U256::from(2), U256::from(3)]).unwrap();
            layout.policy.packed_flags[0].write(0x1111).unwrap();
            layout.policy.packed_flags[16].write(0x2222).unwrap();
            layout.policy.amounts.write(amounts.clone()).unwrap();
            layout.total_supply.write(U256::from(1_000)).unwrap();
            layout.policy_owner.write(policy_owner).unwrap();

            assert_eq!(layout.admin.read().unwrap(), owner);
            assert_eq!(layout.policy.label.read().unwrap(), long_label);
            assert_eq!(layout.policy.balances.at(&owner).read().unwrap(), U256::from(500));
            assert_eq!(layout.policy.checkpoints[2].read().unwrap(), U256::from(3));
            assert_eq!(layout.policy.packed_flags[0].read().unwrap(), 0x1111);
            assert_eq!(layout.policy.packed_flags[16].read().unwrap(), 0x2222);
            assert_eq!(layout.policy.amounts.read().unwrap(), amounts);
            assert_eq!(layout.total_supply.read().unwrap(), U256::from(1_000));
            assert_eq!(layout.policy_owner.read().unwrap(), policy_owner);

            let label_slot =
                slots::POLICY + U256::from(__packing_policy_namespace::LABEL_LOC.offset_slots);
            let balance_slot = owner.mapping_slot(
                slots::POLICY + U256::from(__packing_policy_namespace::BALANCES_LOC.offset_slots),
            );
            let checkpoints_slot = slots::POLICY
                + U256::from(__packing_policy_namespace::CHECKPOINTS_LOC.offset_slots);
            let packed_flags_slot = slots::POLICY
                + U256::from(__packing_policy_namespace::PACKED_FLAGS_LOC.offset_slots);
            let amounts_slot =
                slots::POLICY + U256::from(__packing_policy_namespace::AMOUNTS_LOC.offset_slots);

            assert_eq!(ctx.sload(NAMESPACED_ADDR, balance_slot).unwrap(), U256::from(500));
            assert_eq!(
                ctx.sload(NAMESPACED_ADDR, slots::ADMIN).unwrap(),
                U256::from_be_bytes({
                    let mut word = [0u8; 32];
                    word[12..].copy_from_slice(owner.as_slice());
                    word
                })
            );
            assert_eq!(ctx.sload(NAMESPACED_ADDR, slots::TOTAL_SUPPLY).unwrap(), U256::from(1_000));
            assert_eq!(
                ctx.sload(NAMESPACED_ADDR, slots::POLICY_OWNER).unwrap(),
                U256::from_be_bytes({
                    let mut word = [0u8; 32];
                    word[12..].copy_from_slice(policy_owner.as_slice());
                    word
                })
            );

            assert_eq!(
                ctx.sload(NAMESPACED_ADDR, label_slot).unwrap(),
                U256::from(long_label.len() * 2 + 1)
            );
            let label_data_slot = data_slot(label_slot);
            for chunk_index in 0..long_label.len().div_ceil(32) {
                assert_eq!(
                    ctx.sload(NAMESPACED_ADDR, label_data_slot + U256::from(chunk_index)).unwrap(),
                    word_from_chunk(long_label.as_bytes(), chunk_index)
                );
            }

            assert_eq!(ctx.sload(NAMESPACED_ADDR, checkpoints_slot).unwrap(), U256::from(1));
            assert_eq!(
                ctx.sload(NAMESPACED_ADDR, checkpoints_slot + U256::ONE).unwrap(),
                U256::from(2)
            );
            assert_eq!(
                ctx.sload(NAMESPACED_ADDR, checkpoints_slot + U256::from(2)).unwrap(),
                U256::from(3)
            );

            let packed_first_slot = ctx.sload(NAMESPACED_ADDR, packed_flags_slot).unwrap();
            let packed_second_slot =
                ctx.sload(NAMESPACED_ADDR, packed_flags_slot + U256::ONE).unwrap();
            assert_eq!(packed_first_slot & U256::from(0xffff), U256::from(0x1111));
            assert_eq!(packed_second_slot & U256::from(0xffff), U256::from(0x2222));

            assert_eq!(ctx.sload(NAMESPACED_ADDR, amounts_slot).unwrap(), U256::from(3));
            let amounts_data_slot = data_slot(amounts_slot);
            assert_eq!(ctx.sload(NAMESPACED_ADDR, amounts_data_slot).unwrap(), U256::from(11));
            assert_eq!(
                ctx.sload(NAMESPACED_ADDR, amounts_data_slot + U256::ONE).unwrap(),
                U256::from(22)
            );
            assert_eq!(
                ctx.sload(NAMESPACED_ADDR, amounts_data_slot + U256::from(2)).unwrap(),
                U256::from(33)
            );
        });
    }
}

mod type_namespaced_layouts {
    use alloy_primitives::{Address, U256, address};
    use base_precompile_macros::{Storable, contract};
    use base_precompile_storage::{
        Handler, Mapping, StorableType, StorageCtx, StorageKey, setup_storage,
    };

    use super::erc7201_root;

    const TYPE_NAMESPACE_ADDR: Address = address!("0000000000000000000000000000000000002468");

    /// Core B-20 storage rooted at the canonical B-20 namespace.
    #[derive(Debug, Clone, Storable)]
    #[namespace("b20")]
    struct B20Storage {
        total_supply: U256,
        balances: Mapping<Address, U256>,
    }

    /// Security-specific B-20 extension storage.
    #[derive(Debug, Clone, Storable)]
    #[namespace("b20.security")]
    struct B20SecurityStorage {
        shares_to_tokens_ratio: U256,
        used_announcement_ids: Mapping<String, bool>,
        security_identifiers: Mapping<String, bool>,
    }

    /// Redeem-specific B-20 extension storage.
    #[derive(Debug, Clone, Storable)]
    #[namespace("b20.redeem")]
    struct B20RedeemStorage {
        minimum_redeemable: U256,
        redeem_policy_ids: U256,
    }

    /// Security token layout that composes canonical namespaced storage sections.
    #[contract(addr = TYPE_NAMESPACE_ADDR)]
    pub struct B20SecurityLayout {
        pub local_head: u8,
        pub b20: B20Storage,
        pub security: B20SecurityStorage,
        pub redeem: B20RedeemStorage,
        pub local_tail: u16,
    }

    #[test]
    fn type_level_namespaces_mount_layouts_without_repeating_strings() {
        let b20_value = B20Storage { total_supply: U256::ZERO, balances: Mapping::default() };
        let security_value = B20SecurityStorage {
            shares_to_tokens_ratio: U256::ZERO,
            used_announcement_ids: Mapping::default(),
            security_identifiers: Mapping::default(),
        };
        let redeem_value =
            B20RedeemStorage { minimum_redeemable: U256::ZERO, redeem_policy_ids: U256::ZERO };
        let _ = (
            &b20_value.total_supply,
            &b20_value.balances,
            &security_value.shares_to_tokens_ratio,
            &security_value.used_announcement_ids,
            &security_value.security_identifiers,
            &redeem_value.minimum_redeemable,
            &redeem_value.redeem_policy_ids,
        );

        let b20_root = erc7201_root("b20");
        let security_root = erc7201_root("b20.security");
        let redeem_root = erc7201_root("b20.redeem");

        assert_eq!(<B20Storage as StorableType>::STORAGE_NAMESPACE_ID, "b20");
        assert_eq!(<B20Storage as StorableType>::STORAGE_NAMESPACE_ROOT, b20_root);
        assert_eq!(<B20SecurityStorage as StorableType>::STORAGE_NAMESPACE_ROOT, security_root);
        assert_eq!(<B20RedeemStorage as StorableType>::STORAGE_NAMESPACE_ROOT, redeem_root);

        assert_eq!(slots::LOCAL_HEAD, U256::ZERO);
        assert_eq!(slots::LOCAL_HEAD_OFFSET, 0);
        assert_eq!(slots::B20, b20_root);
        assert_eq!(slots::SECURITY, security_root);
        assert_eq!(slots::REDEEM, redeem_root);
        assert_eq!(slots::LOCAL_TAIL, U256::ZERO);
        assert_eq!(slots::LOCAL_TAIL_OFFSET, 1);
    }

    #[test]
    fn type_level_namespaced_layouts_round_trip_through_handlers() {
        let (mut storage, _) = setup_storage();
        let holder = Address::from([0xaa; 20]);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut layout = B20SecurityLayout::new(ctx);

            layout.local_head.write(0x11).unwrap();
            layout.b20.total_supply.write(U256::from(100)).unwrap();
            layout.b20.balances.at_mut(&holder).write(U256::from(25)).unwrap();
            layout.security.shares_to_tokens_ratio.write(U256::from(2)).unwrap();
            layout.redeem.minimum_redeemable.write(U256::from(10)).unwrap();
            layout.redeem.redeem_policy_ids.write(U256::from(3)).unwrap();
            layout.local_tail.write(0x2233).unwrap();

            assert_eq!(layout.local_head.read().unwrap(), 0x11);
            assert_eq!(layout.b20.total_supply.read().unwrap(), U256::from(100));
            assert_eq!(layout.b20.balances.at(&holder).read().unwrap(), U256::from(25));
            assert_eq!(layout.security.shares_to_tokens_ratio.read().unwrap(), U256::from(2));
            assert_eq!(layout.redeem.minimum_redeemable.read().unwrap(), U256::from(10));
            assert_eq!(layout.redeem.redeem_policy_ids.read().unwrap(), U256::from(3));
            assert_eq!(layout.local_tail.read().unwrap(), 0x2233);

            assert_eq!(
                ctx.sload(
                    TYPE_NAMESPACE_ADDR,
                    slots::B20 + U256::from(__packing_b20_storage::TOTAL_SUPPLY_LOC.offset_slots),
                )
                .unwrap(),
                U256::from(100)
            );
            assert_eq!(
                ctx.sload(
                    TYPE_NAMESPACE_ADDR,
                    holder.mapping_slot(
                        slots::B20 + U256::from(__packing_b20_storage::BALANCES_LOC.offset_slots),
                    ),
                )
                .unwrap(),
                U256::from(25)
            );
            assert_eq!(
                ctx.sload(
                    TYPE_NAMESPACE_ADDR,
                    slots::SECURITY
                        + U256::from(
                            __packing_b20_security_storage::SHARES_TO_TOKENS_RATIO_LOC.offset_slots,
                        ),
                )
                .unwrap(),
                U256::from(2)
            );
            assert_eq!(
                ctx.sload(
                    TYPE_NAMESPACE_ADDR,
                    slots::REDEEM
                        + U256::from(
                            __packing_b20_redeem_storage::MINIMUM_REDEEMABLE_LOC.offset_slots
                        ),
                )
                .unwrap(),
                U256::from(10)
            );
            let local_slot = ctx.sload(TYPE_NAMESPACE_ADDR, slots::LOCAL_HEAD).unwrap();
            assert_eq!(local_slot & U256::from(0xff), U256::from(0x11));
            assert_eq!((local_slot >> 8) & U256::from(0xffff), U256::from(0x2233));
        });
    }
}

mod namespaced_fields {
    use alloy_primitives::{Address, U256, address, uint};
    use base_precompile_macros::contract;
    use base_precompile_storage::{Handler, Mapping, StorageCtx, StorageKey, setup_storage};

    use super::{data_slot, word_from_chunk};

    const FIELD_NAMESPACE_ADDR: Address = address!("0000000000000000000000000000000000008765");
    const EXPECTED_ROOT: U256 =
        uint!(0x50861ae81a7f4392b927efbaeecf8f091f3bd39245aa45ea91499a137b8b3100_U256);

    /// Token storage with individual fields routed into a shared namespace-local layout.
    #[contract(addr = FIELD_NAMESPACE_ADDR)]
    pub struct FieldNamespacedStorage {
        pub admin: Address,
        #[namespace("b20.policy")]
        pub policy_label: String,
        pub total_supply: U256,
        #[namespace("b20.policy")]
        pub policy_balances: Mapping<Address, U256>,
    }

    #[test]
    fn namespaced_fields_share_namespace_layout_without_advancing_contract_slots() {
        assert_eq!(slots::ADMIN, U256::ZERO);
        assert_eq!(slots::POLICY_LABEL, EXPECTED_ROOT);
        assert_eq!(slots::TOTAL_SUPPLY, U256::ONE);
        assert_eq!(slots::POLICY_BALANCES, EXPECTED_ROOT + U256::ONE);

        let (mut storage, _) = setup_storage();
        let owner = Address::from([0xbb; 20]);
        let label = "field-level-namespaced-policy-label-that-spans-two-slots".to_owned();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut layout = FieldNamespacedStorage::new(ctx);
            layout.policy_label.write(label.clone()).unwrap();
            layout.policy_balances.at_mut(&owner).write(U256::from(700)).unwrap();
            layout.total_supply.write(U256::from(2_000)).unwrap();

            assert_eq!(layout.policy_label.read().unwrap(), label);
            assert_eq!(layout.policy_balances.at(&owner).read().unwrap(), U256::from(700));
            assert_eq!(layout.total_supply.read().unwrap(), U256::from(2_000));

            assert_eq!(
                ctx.sload(FIELD_NAMESPACE_ADDR, slots::POLICY_LABEL).unwrap(),
                U256::from(label.len() * 2 + 1)
            );
            let label_data_slot = data_slot(slots::POLICY_LABEL);
            for chunk_index in 0..label.len().div_ceil(32) {
                assert_eq!(
                    ctx.sload(FIELD_NAMESPACE_ADDR, label_data_slot + U256::from(chunk_index))
                        .unwrap(),
                    word_from_chunk(label.as_bytes(), chunk_index)
                );
            }

            let balance_slot = owner.mapping_slot(slots::POLICY_BALANCES);
            assert_eq!(ctx.sload(FIELD_NAMESPACE_ADDR, balance_slot).unwrap(), U256::from(700));
            assert_eq!(
                ctx.sload(FIELD_NAMESPACE_ADDR, slots::TOTAL_SUPPLY).unwrap(),
                U256::from(2_000)
            );
        });
    }
}

mod packed_slot_layout {
    //! End-to-end bit-level verification of multi-field packed storage slots.
    //!
    //! These tests write known sentinel values to each field of a packed slot and
    //! inspect the raw storage word to confirm:
    //! 1. Every field lands at its declared byte offset.
    //! 2. Writing to one field does not bleed into any adjacent field's bit range.
    use alloy_primitives::{Address, U256, address};
    use base_precompile_macros::contract;
    use base_precompile_storage::{Handler, StorageCtx, setup_storage};

    const PACKED_ADDR: Address = address!("0000000000000000000000000000000000009999");

    /// A layout where four sub-word primitives share a single storage slot.
    ///
    /// Packing order (low-to-high bytes within the slot):
    /// - `low_byte`  : u8  → offset_bytes 0, bits  [0..7]
    /// - `mid_short` : u16 → offset_bytes 1, bits  [8..23]
    /// - `mid_int`   : u32 → offset_bytes 3, bits  [24..55]
    /// - `high_long` : u64 → offset_bytes 7, bits  [56..119]
    ///
    /// Total: 15 bytes → all packed into slot 0.
    /// `big_val` (U256, full slot) goes to slot 1.
    #[contract(addr = PACKED_ADDR)]
    pub struct PackedFieldsStorage {
        pub low_byte: u8,
        pub mid_short: u16,
        pub mid_int: u32,
        pub high_long: u64,
        pub big_val: U256,
    }

    #[test]
    fn packed_slot_bit_positions_match_declared_offsets() {
        // Slot constants: all four small fields share slot 0; big_val is slot 1.
        assert_eq!(slots::LOW_BYTE, U256::ZERO);
        assert_eq!(slots::LOW_BYTE_OFFSET, 0);
        assert_eq!(slots::MID_SHORT, U256::ZERO);
        assert_eq!(slots::MID_SHORT_OFFSET, 1);
        assert_eq!(slots::MID_INT, U256::ZERO);
        assert_eq!(slots::MID_INT_OFFSET, 3);
        assert_eq!(slots::HIGH_LONG, U256::ZERO);
        assert_eq!(slots::HIGH_LONG_OFFSET, 7);
        assert_eq!(slots::BIG_VAL, U256::ONE);

        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut layout = PackedFieldsStorage::new(ctx);
            layout.low_byte.write(0xAB_u8).unwrap();
            layout.mid_short.write(0x1234_u16).unwrap();
            layout.mid_int.write(0xDEAD_BEEF_u32).unwrap();
            layout.high_long.write(0xCAFE_BABE_CAFE_BABE_u64).unwrap();

            let raw = ctx.sload(PACKED_ADDR, U256::ZERO).unwrap();

            // offset_bytes=0 → bits [0..7]
            assert_eq!(raw & U256::from(0xFF_u32), U256::from(0xAB_u32), "low_byte at bits [0..7]");
            // offset_bytes=1 → bits [8..23]
            assert_eq!(
                (raw >> 8) & U256::from(0xFFFF_u32),
                U256::from(0x1234_u32),
                "mid_short at bits [8..23]"
            );
            // offset_bytes=3 → bits [24..55]
            assert_eq!(
                (raw >> 24) & U256::from(0xFFFF_FFFF_u32),
                U256::from(0xDEAD_BEEF_u32),
                "mid_int at bits [24..55]"
            );
            // offset_bytes=7 → bits [56..119]
            assert_eq!(
                (raw >> 56) & U256::from(u64::MAX),
                U256::from(0xCAFE_BABE_CAFE_BABE_u64),
                "high_long at bits [56..119]"
            );
            // Bits above high_long (120..255) must be untouched.
            assert_eq!(raw >> 120, U256::ZERO, "bits above high_long must be zero");
        });
    }

    #[test]
    fn packed_slot_write_one_field_does_not_bleed_into_adjacent_fields() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut layout = PackedFieldsStorage::new(ctx);

            // Write only low_byte; all other packed fields must stay zero.
            layout.low_byte.write(0xFF_u8).unwrap();
            let raw = ctx.sload(PACKED_ADDR, U256::ZERO).unwrap();
            assert_eq!(raw & U256::from(0xFF_u32), U256::from(0xFF_u32));
            assert_eq!(
                (raw >> 8) & U256::from(0xFFFF_u32),
                U256::ZERO,
                "mid_short must be zero after writing only low_byte"
            );
            assert_eq!(
                (raw >> 24) & U256::from(0xFFFF_FFFF_u32),
                U256::ZERO,
                "mid_int must be zero"
            );
            assert_eq!((raw >> 56) & U256::from(u64::MAX), U256::ZERO, "high_long must be zero");

            // Write mid_short; low_byte must be preserved exactly.
            layout.mid_short.write(0xABCD_u16).unwrap();
            let raw = ctx.sload(PACKED_ADDR, U256::ZERO).unwrap();
            assert_eq!(
                raw & U256::from(0xFF_u32),
                U256::from(0xFF_u32),
                "low_byte must survive mid_short write"
            );
            assert_eq!((raw >> 8) & U256::from(0xFFFF_u32), U256::from(0xABCD_u32));
            assert_eq!(
                (raw >> 24) & U256::from(0xFFFF_FFFF_u32),
                U256::ZERO,
                "mid_int must still be zero"
            );

            // Overwrite low_byte; mid_short must be preserved exactly.
            layout.low_byte.write(0x42_u8).unwrap();
            let raw = ctx.sload(PACKED_ADDR, U256::ZERO).unwrap();
            assert_eq!(raw & U256::from(0xFF_u32), U256::from(0x42_u32));
            assert_eq!(
                (raw >> 8) & U256::from(0xFFFF_u32),
                U256::from(0xABCD_u32),
                "mid_short must survive low_byte overwrite"
            );

            // Write mid_int; previously written fields must be unchanged.
            layout.mid_int.write(0x1234_5678_u32).unwrap();
            let raw = ctx.sload(PACKED_ADDR, U256::ZERO).unwrap();
            assert_eq!(
                raw & U256::from(0xFF_u32),
                U256::from(0x42_u32),
                "low_byte must survive mid_int write"
            );
            assert_eq!(
                (raw >> 8) & U256::from(0xFFFF_u32),
                U256::from(0xABCD_u32),
                "mid_short must survive mid_int write"
            );
            assert_eq!((raw >> 24) & U256::from(0xFFFF_FFFF_u32), U256::from(0x1234_5678_u32));

            // Write high_long; all lower fields must be preserved exactly.
            layout.high_long.write(0xDEAD_BEEF_DEAD_BEEF_u64).unwrap();
            let raw = ctx.sload(PACKED_ADDR, U256::ZERO).unwrap();
            assert_eq!(
                raw & U256::from(0xFF_u32),
                U256::from(0x42_u32),
                "low_byte must survive high_long write"
            );
            assert_eq!(
                (raw >> 8) & U256::from(0xFFFF_u32),
                U256::from(0xABCD_u32),
                "mid_short must survive high_long write"
            );
            assert_eq!(
                (raw >> 24) & U256::from(0xFFFF_FFFF_u32),
                U256::from(0x1234_5678_u32),
                "mid_int must survive high_long write"
            );
            assert_eq!((raw >> 56) & U256::from(u64::MAX), U256::from(0xDEAD_BEEF_DEAD_BEEF_u64));
        });
    }
}

mod namespace_outer_order {
    use alloy_primitives::{Address, U256, address, uint};
    use base_precompile_macros::{contract, namespace};

    const ORDER_ADDR: Address = address!("0000000000000000000000000000000000005678");

    #[namespace("b20.outer-order")]
    #[contract(addr = ORDER_ADDR)]
    pub struct OuterOrderStorage {
        pub value: U256,
    }

    #[test]
    fn namespace_macro_reorders_above_contract() {
        assert_eq!(ORDER_ADDR, address!("0000000000000000000000000000000000005678"));
        assert_eq!(slots::NAMESPACE_ID, "b20.outer-order");
        assert_eq!(
            slots::NAMESPACE_ROOT,
            uint!(0xf06e16fd945cfdfdb627e60cabea1fb8bb965382c21574655d1e8bb28bdfcf00_U256)
        );
        assert_eq!(slots::VALUE, slots::NAMESPACE_ROOT);
    }
}
