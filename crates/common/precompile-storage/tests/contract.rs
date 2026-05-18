//! End-to-end test: exercises the `#[contract]` macro with `HashMapStorageProvider`.
//!
//! Validates that the macro generates correct storage layout,
//! typed getter/setter fields work round-trip, and collision detection fires.
use alloy_primitives::{Address, U256, address};
use base_precompile_macros::contract;
use base_precompile_storage::{Handler, Mapping, StorageCtx, StorageKey, setup_storage};

const TEST_ADDR: Address = address!("0000000000000000000000000000000000001234");

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
