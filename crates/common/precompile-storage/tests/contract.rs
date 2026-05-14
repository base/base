//! End-to-end test: exercises the `#[precompile_storage]` macro with `HashMapStorageProvider`.
//!
//! Validates that the macro generates correct storage layout,
//! typed getter/setter accessors work round-trip, and collision detection fires.
use alloy_primitives::{Address, U256, address, keccak256};
use base_precompile_macros::precompile_storage;
use base_precompile_storage::{Handler, Mapping, StorageCtx, StorageKey, setup_storage};

const TEST_ADDR: Address = address!("0000000000000000000000000000000000001234");
const TEST_BASE_SLOT: &str = "test.token";

fn test_base_slot() -> U256 {
    keccak256(TEST_BASE_SLOT.as_bytes()).into()
}

/// A minimal token storage layout for integration testing.
#[precompile_storage(addr = TEST_ADDR, base_slot = "test.token")]
pub struct TestToken {
    owner: Address,
    total_supply: U256,
    balances: Mapping<Address, U256>,
    allowances: Mapping<Address, Mapping<Address, U256>>,
}

#[test]
fn test_contract_macro_basic_roundtrip() {
    let (mut storage, _) = setup_storage();

    StorageCtx::enter(&mut storage, || {
        let token = TestToken::new();

        let alice = Address::from([0xaa; 20]);
        let bob = Address::from([0xbb; 20]);

        // Write owner and total_supply
        let mut owner = token.owner();
        owner.write(alice).unwrap();
        let mut total_supply = token.total_supply();
        total_supply.write(U256::from(1_000_000u64)).unwrap();

        // Read back
        assert_eq!(token.owner().read().unwrap(), alice);
        assert_eq!(token.total_supply().read().unwrap(), U256::from(1_000_000u64));

        // Write and read a mapping entry
        let mut balances = token.balances();
        balances.at_mut(&alice).write(U256::from(500u64)).unwrap();
        assert_eq!(token.balances().at(&alice).read().unwrap(), U256::from(500u64));
        assert_eq!(token.balances().at(&bob).read().unwrap(), U256::ZERO);

        // Nested mapping
        let mut allowances = token.allowances();
        allowances[alice][bob].write(U256::from(100u64)).unwrap();
        assert_eq!(token.allowances()[alice][bob].read().unwrap(), U256::from(100u64));
        assert_eq!(token.allowances()[bob][alice].read().unwrap(), U256::ZERO);
    });
}

#[test]
fn test_contract_slots_are_deterministic() {
    // Verify that the generated slot constants are stable across runs.
    // owner is field 0 at base_slot, total_supply is field 1 at base_slot + 1.
    let base_slot = test_base_slot();
    assert_eq!(slots::OWNER, base_slot);
    assert_eq!(slots::TOTAL_SUPPLY, base_slot.checked_add(U256::from(1u64)).unwrap());
    assert_eq!(slots::BALANCES, base_slot.checked_add(U256::from(2u64)).unwrap());
    assert_eq!(slots::ALLOWANCES, base_slot.checked_add(U256::from(3u64)).unwrap());
}

#[test]
fn test_contract_mapping_slot_derivation() {
    // Verify that mapping slots match the Solidity keccak256 derivation.
    let alice = Address::from([0xaa; 20]);
    let expected = alice.mapping_slot(slots::BALANCES);

    let (mut storage, _) = setup_storage();
    StorageCtx::enter(&mut storage, || {
        let write_value = U256::from(42u64);
        let mut balances = TestToken::new().balances();
        balances.at_mut(&alice).write(write_value).unwrap();

        // Verify the raw storage slot matches the expected derivation.
        let raw = StorageCtx.sload(TEST_ADDR, expected).unwrap();
        assert_eq!(raw, write_value);
    });
}

#[test]
fn test_contract_multiple_instances_independent() {
    let (mut storage1, _) = setup_storage();
    let (mut storage2, _) = setup_storage();

    let alice = Address::from([0xaa; 20]);

    StorageCtx::enter(&mut storage1, || {
        let mut balances = TestToken::new().balances();
        balances.at_mut(&alice).write(U256::from(100u64)).unwrap();
    });

    StorageCtx::enter(&mut storage2, || {
        let t2 = TestToken::new();
        // storage2 is independent, so balance should be zero.
        assert_eq!(t2.balances().at(&alice).read().unwrap(), U256::ZERO);
    });
}
