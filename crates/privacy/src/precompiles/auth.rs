//! Auth Precompile (0x0201)
//!
//! Allows slot owners to grant and revoke read access to their private data.
//!
//! # Functions
//!
//! - `grant(address contract, uint256 slot, address grantee, uint8 permission)`: Grant access
//! - `revoke(address contract, uint256 slot, address grantee)`: Revoke access
//! - `isAuthorized(address contract, uint256 slot, address query)`: Check authorization

use alloy_primitives::Bytes;
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};

use super::context::with_context;
use super::encoding::{decode_auth_call, AuthCall};
use super::error::{encode_revert_message, encode_success, PrecompileInputError};
use super::AUTH_BASE_GAS;
use crate::store::{AuthEntry, READ};

/// Execute the auth precompile.
///
/// This is the main entry point called by the EVM when address 0x201 is invoked.
pub fn run(input: &[u8], gas_limit: u64) -> PrecompileResult {
    // Decode the call
    let call = match decode_auth_call(input) {
        Ok(c) => c,
        Err(e) => {
            return Ok(PrecompileOutput::new_reverted(
                AUTH_BASE_GAS.min(gas_limit),
                Bytes::from(encode_revert_message(&e.to_string())),
            ));
        }
    };

    // Execute based on call type
    match call {
        AuthCall::Grant(grant) => execute_grant(grant, gas_limit),
        AuthCall::Revoke(revoke) => execute_revoke(revoke, gas_limit),
        AuthCall::IsAuthorized(query) => execute_is_authorized(query, gas_limit),
    }
}

/// Execute the `grant` function.
fn execute_grant(
    input: super::encoding::AuthGrantInput,
    gas_limit: u64,
) -> PrecompileResult {
    if gas_limit < AUTH_BASE_GAS {
        return Err(PrecompileError::OutOfGas);
    }

    let result = with_context(|ctx| {
        let caller = ctx.caller;

        // Check if the contract is registered
        let config = ctx
            .registry
            .get_config(&input.contract)
            .ok_or(PrecompileInputError::NotRegistered)?;

        // Get the owner of this slot
        let slot_owner = ctx.store.get_owner(input.contract, input.slot);

        // The caller must be the owner of this slot
        // If no owner is recorded, only the contract or admin can grant
        let is_authorized = match slot_owner {
            Some(owner) => caller == owner,
            None => caller == input.contract || caller == config.admin,
        };

        if !is_authorized {
            return Err(PrecompileInputError::NotOwner);
        }

        // Grant the authorization, merging with any existing permissions
        let existing_permissions = ctx
            .store
            .get_authorization(input.contract, input.slot, input.grantee)
            .map(|a| a.permissions)
            .unwrap_or(0);

        let auth = AuthEntry {
            permissions: existing_permissions | input.permission, // Merge permissions
            expiry: 0, // Never expires
            granted_at: ctx.block,
        };
        ctx.store.authorize(
            input.contract,
            input.slot,
            input.grantee,
            auth,
        );

        Ok(())
    });

    match result {
        Some(Ok(())) => Ok(PrecompileOutput::new(AUTH_BASE_GAS, Bytes::from(encode_success()))),
        Some(Err(e)) => Ok(PrecompileOutput::new_reverted(
            AUTH_BASE_GAS,
            Bytes::from(encode_revert_message(&e.to_string())),
        )),
        None => Ok(PrecompileOutput::new_reverted(
            AUTH_BASE_GAS,
            Bytes::from(encode_revert_message("context not available")),
        )),
    }
}

/// Execute the `revoke` function.
fn execute_revoke(
    input: super::encoding::AuthRevokeInput,
    gas_limit: u64,
) -> PrecompileResult {
    if gas_limit < AUTH_BASE_GAS {
        return Err(PrecompileError::OutOfGas);
    }

    let result = with_context(|ctx| {
        let caller = ctx.caller;

        // Check if the contract is registered
        let config = ctx
            .registry
            .get_config(&input.contract)
            .ok_or(PrecompileInputError::NotRegistered)?;

        // Get the owner of this slot
        let slot_owner = ctx.store.get_owner(input.contract, input.slot);

        // The caller must be the owner of this slot
        let is_authorized = match slot_owner {
            Some(owner) => caller == owner,
            None => caller == input.contract || caller == config.admin,
        };

        if !is_authorized {
            return Err(PrecompileInputError::NotOwner);
        }

        // Revoke the authorization
        ctx.store.revoke(input.contract, input.slot, input.grantee);

        Ok(())
    });

    match result {
        Some(Ok(())) => Ok(PrecompileOutput::new(AUTH_BASE_GAS, Bytes::from(encode_success()))),
        Some(Err(e)) => Ok(PrecompileOutput::new_reverted(
            AUTH_BASE_GAS,
            Bytes::from(encode_revert_message(&e.to_string())),
        )),
        None => Ok(PrecompileOutput::new_reverted(
            AUTH_BASE_GAS,
            Bytes::from(encode_revert_message("context not available")),
        )),
    }
}

/// Execute the `isAuthorized` function.
fn execute_is_authorized(
    input: super::encoding::IsAuthorizedInput,
    gas_limit: u64,
) -> PrecompileResult {
    const IS_AUTHORIZED_GAS: u64 = 100;

    if gas_limit < IS_AUTHORIZED_GAS {
        return Err(PrecompileError::OutOfGas);
    }

    let result = with_context(|ctx| {
        // Check if the query address is authorized for READ
        let is_authorized = ctx.store.is_authorized(
            input.contract,
            input.slot,
            input.query,
            READ,
        );
        super::error::encode_bool(is_authorized)
    });

    match result {
        Some(encoded) => Ok(PrecompileOutput::new(IS_AUTHORIZED_GAS, Bytes::from(encoded))),
        None => Ok(PrecompileOutput::new_reverted(
            IS_AUTHORIZED_GAS,
            Bytes::from(encode_revert_message("context not available")),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::precompiles::context::{clear_context, set_context, PrecompileContext};
    use crate::precompiles::encoding::{
        build_grant_input, build_is_authorized_input, build_revoke_input,
    };
    use crate::registry::{OwnershipType, PrivateContractConfig, SlotConfig, SlotType};
    use crate::store::{READ, WRITE};
    use crate::{PrivacyRegistry, PrivateStateStore};
    use alloy_primitives::{Address, U256};
    use std::sync::Arc;

    fn test_address(n: u8) -> Address {
        Address::new([n; 20])
    }

    fn setup_context(caller: Address) -> (Arc<PrivacyRegistry>, Arc<PrivateStateStore>) {
        let registry = Arc::new(PrivacyRegistry::new());
        let store = Arc::new(PrivateStateStore::new());

        set_context(PrecompileContext::new(
            Arc::clone(&registry),
            Arc::clone(&store),
            caller,
            100,
        ));

        (registry, store)
    }

    fn register_contract(
        registry: &PrivacyRegistry,
        contract: Address,
        admin: Address,
    ) {
        let config = PrivateContractConfig {
            address: contract,
            admin,
            slots: vec![SlotConfig {
                base_slot: U256::ZERO,
                slot_type: SlotType::Mapping,
                ownership: OwnershipType::MappingKey,
            }],
            registered_at: 1,
            hide_events: false,
        };
        registry.register(config).unwrap();
    }

    fn teardown() {
        clear_context();
    }

    /// Helper to create an AuthEntry for testing
    fn make_auth(permission: u8) -> AuthEntry {
        AuthEntry {
            permissions: permission,
            expiry: 0,
            granted_at: 1,
        }
    }

    // ========================================================================
    // Grant Tests
    // ========================================================================

    #[test]
    fn test_grant_by_slot_owner() {
        let contract = test_address(1);
        let admin = test_address(2);
        let alice = test_address(10); // slot owner
        let bob = test_address(20); // grantee
        let slot = U256::from(100);

        // Alice is the caller and slot owner
        let (registry, store) = setup_context(alice);
        register_contract(&registry, contract, admin);

        // Set Alice as owner of this slot
        store.set(contract, slot, U256::from(42), alice);

        let input = build_grant_input(contract, slot, bob, READ);
        let result = run(&input, 100_000).unwrap();

        assert!(!result.reverted);

        // Verify Bob is now authorized
        assert!(store.is_authorized(contract, slot, bob, READ));

        teardown();
    }

    #[test]
    fn test_grant_by_non_owner_fails() {
        let contract = test_address(1);
        let admin = test_address(2);
        let alice = test_address(10); // slot owner
        let eve = test_address(30); // attacker
        let bob = test_address(20);
        let slot = U256::from(100);

        // Eve is the caller (not the owner)
        let (registry, store) = setup_context(eve);
        register_contract(&registry, contract, admin);

        // Set Alice as owner of this slot
        store.set(contract, slot, U256::from(42), alice);

        let input = build_grant_input(contract, slot, bob, READ);
        let result = run(&input, 100_000).unwrap();

        assert!(result.reverted);

        // Bob should NOT be authorized
        assert!(!store.is_authorized(contract, slot, bob, READ));

        teardown();
    }

    #[test]
    fn test_grant_by_admin_for_unowned_slot() {
        let contract = test_address(1);
        let admin = test_address(2);
        let bob = test_address(20);
        let slot = U256::from(100);

        // Admin is the caller
        let (registry, store) = setup_context(admin);
        register_contract(&registry, contract, admin);

        // No owner recorded for this slot
        let input = build_grant_input(contract, slot, bob, READ);
        let result = run(&input, 100_000).unwrap();

        assert!(!result.reverted);
        assert!(store.is_authorized(contract, slot, bob, READ));

        teardown();
    }

    #[test]
    fn test_grant_by_contract_for_unowned_slot() {
        let contract = test_address(1);
        let admin = test_address(2);
        let bob = test_address(20);
        let slot = U256::from(100);

        // Contract is the caller
        let (registry, store) = setup_context(contract);
        register_contract(&registry, contract, admin);

        // No owner recorded for this slot
        let input = build_grant_input(contract, slot, bob, WRITE);
        let result = run(&input, 100_000).unwrap();

        assert!(!result.reverted);
        assert!(store.is_authorized(contract, slot, bob, WRITE));

        teardown();
    }

    #[test]
    fn test_grant_unregistered_contract_fails() {
        let contract = test_address(1);
        let bob = test_address(20);
        let slot = U256::from(100);

        // Caller is the contract, but it's not registered
        setup_context(contract);

        let input = build_grant_input(contract, slot, bob, READ);
        let result = run(&input, 100_000).unwrap();

        assert!(result.reverted);

        teardown();
    }

    // ========================================================================
    // Revoke Tests
    // ========================================================================

    #[test]
    fn test_revoke_by_owner() {
        let contract = test_address(1);
        let admin = test_address(2);
        let alice = test_address(10);
        let bob = test_address(20);
        let slot = U256::from(100);

        let (registry, store) = setup_context(alice);
        register_contract(&registry, contract, admin);

        // Alice owns the slot and grants to Bob
        store.set(contract, slot, U256::from(42), alice);
        store.authorize(contract, slot, bob, make_auth(READ));

        // Verify Bob is authorized
        assert!(store.is_authorized(contract, slot, bob, READ));

        // Alice revokes
        let input = build_revoke_input(contract, slot, bob);
        let result = run(&input, 100_000).unwrap();

        assert!(!result.reverted);
        assert!(!store.is_authorized(contract, slot, bob, READ));

        teardown();
    }

    #[test]
    fn test_revoke_by_non_owner_fails() {
        let contract = test_address(1);
        let admin = test_address(2);
        let alice = test_address(10);
        let bob = test_address(20);
        let eve = test_address(30);
        let slot = U256::from(100);

        // Start as Alice to grant
        let (registry, store) = setup_context(alice);
        register_contract(&registry, contract, admin);

        store.set(contract, slot, U256::from(42), alice);
        store.authorize(contract, slot, bob, make_auth(READ));

        // Now Eve tries to revoke
        clear_context();
        set_context(PrecompileContext::new(
            registry,
            Arc::clone(&store),
            eve,
            100,
        ));

        let input = build_revoke_input(contract, slot, bob);
        let result = run(&input, 100_000).unwrap();

        assert!(result.reverted);

        // Bob should still be authorized
        assert!(store.is_authorized(contract, slot, bob, READ));

        teardown();
    }

    // ========================================================================
    // IsAuthorized Tests
    // ========================================================================

    #[test]
    fn test_is_authorized_true() {
        let contract = test_address(1);
        let admin = test_address(2);
        let alice = test_address(10);
        let bob = test_address(20);
        let slot = U256::from(100);

        let (registry, store) = setup_context(alice);
        register_contract(&registry, contract, admin);

        store.set(contract, slot, U256::from(42), alice);
        store.authorize(contract, slot, bob, make_auth(READ));

        let input = build_is_authorized_input(contract, slot, bob);
        let result = run(&input, 100_000).unwrap();

        assert!(!result.reverted);
        assert_eq!(result.bytes.len(), 32);
        assert_eq!(result.bytes[31], 1); // true

        teardown();
    }

    #[test]
    fn test_is_authorized_false() {
        let contract = test_address(1);
        let admin = test_address(2);
        let alice = test_address(10);
        let bob = test_address(20);
        let slot = U256::from(100);

        let (_registry, _store) = setup_context(alice);
        register_contract(&_registry, contract, admin);

        // Bob is NOT authorized
        let input = build_is_authorized_input(contract, slot, bob);
        let result = run(&input, 100_000).unwrap();

        assert!(!result.reverted);
        assert_eq!(result.bytes.len(), 32);
        assert_eq!(result.bytes[31], 0); // false

        teardown();
    }

    #[test]
    fn test_owner_is_always_authorized() {
        let contract = test_address(1);
        let admin = test_address(2);
        let alice = test_address(10);
        let slot = U256::from(100);

        let (registry, store) = setup_context(alice);
        register_contract(&registry, contract, admin);

        store.set(contract, slot, U256::from(42), alice);

        // Owner is always authorized even without explicit grant
        let input = build_is_authorized_input(contract, slot, alice);
        let result = run(&input, 100_000).unwrap();

        assert!(!result.reverted);
        assert_eq!(result.bytes[31], 1); // true

        teardown();
    }

    // ========================================================================
    // Gas Tests
    // ========================================================================

    #[test]
    fn test_grant_out_of_gas() {
        let contract = test_address(1);
        let bob = test_address(20);
        let slot = U256::from(100);

        setup_context(contract);

        let input = build_grant_input(contract, slot, bob, READ);
        let result = run(&input, 100); // Very low gas

        assert!(matches!(result, Err(PrecompileError::OutOfGas)));

        teardown();
    }

    // ========================================================================
    // Edge Cases
    // ========================================================================

    #[test]
    fn test_grant_to_self() {
        let contract = test_address(1);
        let admin = test_address(2);
        let alice = test_address(10);
        let slot = U256::from(100);

        let (registry, store) = setup_context(alice);
        register_contract(&registry, contract, admin);

        store.set(contract, slot, U256::from(42), alice);

        // Alice grants to herself (should work)
        let input = build_grant_input(contract, slot, alice, READ);
        let result = run(&input, 100_000).unwrap();

        assert!(!result.reverted);

        teardown();
    }

    #[test]
    fn test_grant_multiple_permissions() {
        let contract = test_address(1);
        let admin = test_address(2);
        let alice = test_address(10);
        let bob = test_address(20);
        let slot = U256::from(100);

        let (registry, store) = setup_context(alice);
        register_contract(&registry, contract, admin);

        store.set(contract, slot, U256::from(42), alice);

        // Grant READ first
        let input = build_grant_input(contract, slot, bob, READ);
        run(&input, 100_000).unwrap();

        // Grant WRITE
        let input = build_grant_input(contract, slot, bob, WRITE);
        run(&input, 100_000).unwrap();

        // Bob should have both
        assert!(store.is_authorized(contract, slot, bob, READ));
        assert!(store.is_authorized(contract, slot, bob, WRITE));

        teardown();
    }
}
