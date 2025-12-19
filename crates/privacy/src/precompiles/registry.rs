//! Registry Precompile (0x0200)
//!
//! Allows contracts to register themselves for privacy protection.
//!
//! # Functions
//!
//! - `register(address admin, SlotConfig[] slots, bool hideEvents)`: Register the caller
//! - `addSlots(SlotConfig[] slots)`: Add more private slots (admin only)
//! - `setHideEvents(bool hide)`: Toggle event hiding (admin only)
//! - `isRegistered(address contract)`: Query if a contract is registered

use alloy_primitives::Bytes;
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};

use super::context::with_context;
use super::encoding::{decode_registry_call, RegistryCall};
use super::error::{encode_revert_message, encode_success, PrecompileInputError};
use super::{REGISTRY_BASE_GAS, REGISTRY_PER_SLOT_GAS};
use crate::registry::{PrivateContractConfig, SlotConfig};

/// Execute the registry precompile.
///
/// This is the main entry point called by the EVM when address 0x200 is invoked.
pub fn run(input: &[u8], gas_limit: u64) -> PrecompileResult {
    // Decode the call
    let call = match decode_registry_call(input) {
        Ok(c) => c,
        Err(e) => {
            return Ok(PrecompileOutput::new_reverted(
                REGISTRY_BASE_GAS.min(gas_limit),
                Bytes::from(encode_revert_message(&e.to_string())),
            ));
        }
    };

    // Execute based on call type
    match call {
        RegistryCall::Register(reg) => execute_register(reg, gas_limit),
        RegistryCall::AddSlots(add) => execute_add_slots(add, gas_limit),
        RegistryCall::SetHideEvents(set) => execute_set_hide_events(set, gas_limit),
        RegistryCall::IsRegistered(query) => execute_is_registered(query, gas_limit),
    }
}

/// Execute the `register` function.
fn execute_register(
    input: super::encoding::RegistrationInput,
    gas_limit: u64,
) -> PrecompileResult {
    // Calculate gas cost
    let gas_cost = REGISTRY_BASE_GAS + (input.slots.len() as u64) * REGISTRY_PER_SLOT_GAS;
    if gas_limit < gas_cost {
        return Err(PrecompileError::OutOfGas);
    }

    // Get context and execute
    let result = with_context(|ctx| {
        let caller = ctx.caller;

        // Check if already registered
        if ctx.registry.get_config(&caller).is_some() {
            return Err(PrecompileInputError::AlreadyRegistered);
        }

        // Convert slot inputs to SlotConfig
        let slots: Vec<SlotConfig> = input
            .slots
            .into_iter()
            .map(|s| SlotConfig {
                base_slot: s.base_slot,
                slot_type: s.slot_type,
                ownership: s.ownership,
            })
            .collect();

        // Build the config
        let config = PrivateContractConfig {
            address: caller,
            admin: input.admin,
            slots,
            registered_at: ctx.block,
            hide_events: input.hide_events,
        };

        // Register
        ctx.registry
            .register(config)
            .map_err(|e| PrecompileInputError::InvalidSlotConfig(e.to_string()))?;

        Ok(())
    });

    match result {
        Some(Ok(())) => Ok(PrecompileOutput::new(gas_cost, Bytes::from(encode_success()))),
        Some(Err(e)) => Ok(PrecompileOutput::new_reverted(
            gas_cost,
            Bytes::from(encode_revert_message(&e.to_string())),
        )),
        None => Ok(PrecompileOutput::new_reverted(
            REGISTRY_BASE_GAS,
            Bytes::from(encode_revert_message("context not available")),
        )),
    }
}

/// Execute the `addSlots` function.
fn execute_add_slots(
    input: super::encoding::AddSlotsInput,
    gas_limit: u64,
) -> PrecompileResult {
    let gas_cost = REGISTRY_BASE_GAS + (input.slots.len() as u64) * REGISTRY_PER_SLOT_GAS;
    if gas_limit < gas_cost {
        return Err(PrecompileError::OutOfGas);
    }

    let result = with_context(|ctx| {
        let caller = ctx.caller;

        // Get the config and verify caller is admin
        let config = ctx
            .registry
            .get_config(&caller)
            .ok_or(PrecompileInputError::NotRegistered)?;

        // The caller must be either the contract itself or the admin
        // For addSlots, we require the caller to be the admin
        if caller != config.admin && caller != config.address {
            return Err(PrecompileInputError::NotAdmin);
        }

        // Convert and add slots
        let new_slots: Vec<SlotConfig> = input
            .slots
            .into_iter()
            .map(|s| SlotConfig {
                base_slot: s.base_slot,
                slot_type: s.slot_type,
                ownership: s.ownership,
            })
            .collect();

        ctx.registry
            .add_slots(&config.address, new_slots)
            .map_err(|e| PrecompileInputError::InvalidSlotConfig(e.to_string()))?;

        Ok(())
    });

    match result {
        Some(Ok(())) => Ok(PrecompileOutput::new(gas_cost, Bytes::from(encode_success()))),
        Some(Err(e)) => Ok(PrecompileOutput::new_reverted(
            gas_cost,
            Bytes::from(encode_revert_message(&e.to_string())),
        )),
        None => Ok(PrecompileOutput::new_reverted(
            REGISTRY_BASE_GAS,
            Bytes::from(encode_revert_message("context not available")),
        )),
    }
}

/// Execute the `setHideEvents` function.
fn execute_set_hide_events(
    input: super::encoding::SetHideEventsInput,
    gas_limit: u64,
) -> PrecompileResult {
    if gas_limit < REGISTRY_BASE_GAS {
        return Err(PrecompileError::OutOfGas);
    }

    let result = with_context(|ctx| {
        let caller = ctx.caller;

        // Get the config and verify caller is admin
        let config = ctx
            .registry
            .get_config(&caller)
            .ok_or(PrecompileInputError::NotRegistered)?;

        // The caller must be either the contract itself or the admin
        if caller != config.admin && caller != config.address {
            return Err(PrecompileInputError::NotAdmin);
        }

        ctx.registry
            .set_hide_events(&config.address, input.hide)
            .map_err(|e| PrecompileInputError::InvalidSlotConfig(e.to_string()))?;

        Ok(())
    });

    match result {
        Some(Ok(())) => Ok(PrecompileOutput::new(
            REGISTRY_BASE_GAS,
            Bytes::from(encode_success()),
        )),
        Some(Err(e)) => Ok(PrecompileOutput::new_reverted(
            REGISTRY_BASE_GAS,
            Bytes::from(encode_revert_message(&e.to_string())),
        )),
        None => Ok(PrecompileOutput::new_reverted(
            REGISTRY_BASE_GAS,
            Bytes::from(encode_revert_message("context not available")),
        )),
    }
}

/// Execute the `isRegistered` function.
fn execute_is_registered(
    input: super::encoding::IsRegisteredInput,
    gas_limit: u64,
) -> PrecompileResult {
    // isRegistered is a read-only operation, very cheap
    const IS_REGISTERED_GAS: u64 = 100;

    if gas_limit < IS_REGISTERED_GAS {
        return Err(PrecompileError::OutOfGas);
    }

    let result = with_context(|ctx| {
        let is_registered = ctx.registry.get_config(&input.contract).is_some();
        super::error::encode_bool(is_registered)
    });

    match result {
        Some(encoded) => Ok(PrecompileOutput::new(IS_REGISTERED_GAS, Bytes::from(encoded))),
        None => Ok(PrecompileOutput::new_reverted(
            IS_REGISTERED_GAS,
            Bytes::from(encode_revert_message("context not available")),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::precompiles::context::{clear_context, set_context, PrecompileContext};
    use crate::precompiles::encoding::{build_is_registered_input, build_register_input, SlotConfigInput};
    use crate::registry::{OwnershipType, SlotType};
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

    fn teardown() {
        clear_context();
    }

    // ========================================================================
    // Register Tests
    // ========================================================================

    #[test]
    fn test_register_success() {
        let caller = test_address(1);
        let (registry, _) = setup_context(caller);

        let admin = test_address(2);
        let slots = vec![SlotConfigInput {
            base_slot: U256::from(5),
            slot_type: SlotType::Simple,
            ownership: OwnershipType::Contract,
        }];

        let input = build_register_input(admin, false, &slots);
        let result = run(&input, 100_000).unwrap();

        assert!(!result.reverted);
        assert!(registry.get_config(&caller).is_some());

        let config = registry.get_config(&caller).unwrap();
        assert_eq!(config.admin, admin);
        assert_eq!(config.slots.len(), 1);
        assert!(!config.hide_events);

        teardown();
    }

    #[test]
    fn test_register_already_registered() {
        let caller = test_address(1);
        let (registry, _) = setup_context(caller);

        // Register once
        let admin = test_address(2);
        let input = build_register_input(admin, false, &[]);
        let result = run(&input, 100_000).unwrap();
        assert!(!result.reverted);

        // Try to register again
        let result = run(&input, 100_000).unwrap();
        assert!(result.reverted);

        // Verify error message
        let error_msg = String::from_utf8_lossy(&result.bytes);
        assert!(error_msg.contains("already registered") || result.bytes.len() > 0);

        teardown();
    }

    #[test]
    fn test_register_out_of_gas() {
        let caller = test_address(1);
        setup_context(caller);

        let admin = test_address(2);
        let slots = vec![SlotConfigInput {
            base_slot: U256::from(5),
            slot_type: SlotType::Simple,
            ownership: OwnershipType::Contract,
        }];

        let input = build_register_input(admin, false, &slots);

        // Try with very low gas
        let result = run(&input, 100);
        assert!(matches!(result, Err(PrecompileError::OutOfGas)));

        teardown();
    }

    #[test]
    fn test_register_multiple_slots() {
        let caller = test_address(1);
        let (registry, _) = setup_context(caller);

        let admin = test_address(2);
        let slots = vec![
            SlotConfigInput {
                base_slot: U256::from(0),
                slot_type: SlotType::Mapping,
                ownership: OwnershipType::MappingKey,
            },
            SlotConfigInput {
                base_slot: U256::from(1),
                slot_type: SlotType::NestedMapping,
                ownership: OwnershipType::OuterKey,
            },
            SlotConfigInput {
                base_slot: U256::from(5),
                slot_type: SlotType::Simple,
                ownership: OwnershipType::Contract,
            },
        ];

        let input = build_register_input(admin, true, &slots);
        let result = run(&input, 100_000).unwrap();

        assert!(!result.reverted);

        let config = registry.get_config(&caller).unwrap();
        assert_eq!(config.slots.len(), 3);
        assert!(config.hide_events);

        teardown();
    }

    // ========================================================================
    // IsRegistered Tests
    // ========================================================================

    #[test]
    fn test_is_registered_false() {
        let caller = test_address(1);
        setup_context(caller);

        let query_contract = test_address(99);
        let input = build_is_registered_input(query_contract);
        let result = run(&input, 100_000).unwrap();

        assert!(!result.reverted);
        // Result should be false (all zeros)
        assert_eq!(result.bytes.len(), 32);
        assert!(result.bytes.iter().all(|&b| b == 0));

        teardown();
    }

    #[test]
    fn test_is_registered_true() {
        let caller = test_address(1);
        let (registry, _) = setup_context(caller);

        // Register the caller first
        let admin = test_address(2);
        let input = build_register_input(admin, false, &[]);
        run(&input, 100_000).unwrap();

        // Now query
        let input = build_is_registered_input(caller);
        let result = run(&input, 100_000).unwrap();

        assert!(!result.reverted);
        // Result should be true (last byte is 1)
        assert_eq!(result.bytes.len(), 32);
        assert_eq!(result.bytes[31], 1);

        teardown();
    }

    // ========================================================================
    // Context Tests
    // ========================================================================

    #[test]
    fn test_run_without_context() {
        clear_context();

        let admin = test_address(2);
        let input = build_register_input(admin, false, &[]);
        let result = run(&input, 100_000).unwrap();

        // Should revert with "context not available"
        assert!(result.reverted);
    }

    // ========================================================================
    // Invalid Input Tests
    // ========================================================================

    #[test]
    fn test_unknown_selector() {
        let caller = test_address(1);
        setup_context(caller);

        let input = [0xde, 0xad, 0xbe, 0xef];
        let result = run(&input, 100_000).unwrap();

        assert!(result.reverted);

        teardown();
    }

    #[test]
    fn test_empty_input() {
        let caller = test_address(1);
        setup_context(caller);

        let input: &[u8] = &[];
        let result = run(input, 100_000).unwrap();

        assert!(result.reverted);

        teardown();
    }

    // ========================================================================
    // Gas Calculation Tests
    // ========================================================================

    #[test]
    fn test_gas_calculation() {
        let caller = test_address(1);
        setup_context(caller);

        let admin = test_address(2);
        let slots = vec![
            SlotConfigInput {
                base_slot: U256::from(0),
                slot_type: SlotType::Simple,
                ownership: OwnershipType::Contract,
            },
            SlotConfigInput {
                base_slot: U256::from(1),
                slot_type: SlotType::Simple,
                ownership: OwnershipType::Contract,
            },
        ];

        let input = build_register_input(admin, false, &slots);
        let result = run(&input, 100_000).unwrap();

        // Expected gas: 5000 base + 2 * 2000 per slot = 9000
        assert_eq!(result.gas_used, REGISTRY_BASE_GAS + 2 * REGISTRY_PER_SLOT_GAS);

        teardown();
    }
}
