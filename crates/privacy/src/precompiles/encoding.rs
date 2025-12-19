//! ABI Encoding/Decoding for Privacy Precompiles
//!
//! This module handles the encoding and decoding of inputs for the privacy
//! precompiles using Solidity ABI encoding for compatibility with smart contracts.

use crate::registry::{OwnershipType, SlotType};
use alloy_primitives::{Address, U256};

use super::constants::*;
use super::error::PrecompileInputError;

// ============================================================================
// Input Types
// ============================================================================

/// Decoded input for the `register` function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationInput {
    /// Admin address for the contract
    pub admin: Address,
    /// Slot configurations
    pub slots: Vec<SlotConfigInput>,
    /// Whether to hide events
    pub hide_events: bool,
}

/// Single slot configuration input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotConfigInput {
    /// Base storage slot
    pub base_slot: U256,
    /// Type of slot (Simple, Mapping, etc.)
    pub slot_type: SlotType,
    /// Ownership model
    pub ownership: OwnershipType,
}

/// Decoded input for the `addSlots` function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AddSlotsInput {
    /// Slot configurations to add
    pub slots: Vec<SlotConfigInput>,
}

/// Decoded input for `setHideEvents` function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SetHideEventsInput {
    /// Whether to hide events
    pub hide: bool,
}

/// Decoded input for `isRegistered` function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct IsRegisteredInput {
    /// Contract address to query
    pub contract: Address,
}

/// Decoded input for `grant` function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthGrantInput {
    /// Contract address
    pub contract: Address,
    /// Storage slot
    pub slot: U256,
    /// Address to grant access to
    pub grantee: Address,
    /// Permission level (e.g., READ = 1, WRITE = 2)
    pub permission: u8,
}

/// Decoded input for `revoke` function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthRevokeInput {
    /// Contract address
    pub contract: Address,
    /// Storage slot
    pub slot: U256,
    /// Address to revoke access from
    pub grantee: Address,
}

/// Decoded input for `isAuthorized` function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsAuthorizedInput {
    /// Contract address
    pub contract: Address,
    /// Storage slot
    pub slot: U256,
    /// Address to check
    pub query: Address,
}

// ============================================================================
// Dispatching
// ============================================================================

/// Registry function call variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RegistryCall {
    Register(RegistrationInput),
    AddSlots(AddSlotsInput),
    SetHideEvents(SetHideEventsInput),
    IsRegistered(IsRegisteredInput),
}

/// Auth function call variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AuthCall {
    Grant(AuthGrantInput),
    Revoke(AuthRevokeInput),
    IsAuthorized(IsAuthorizedInput),
}

// ============================================================================
// Decoding Helpers
// ============================================================================

/// Read a 32-byte word as an address (right-aligned, left 12 bytes are zero).
fn read_address(data: &[u8], offset: usize) -> Result<Address, PrecompileInputError> {
    if data.len() < offset + 32 {
        return Err(PrecompileInputError::InputTooShort {
            expected: offset + 32,
            actual: data.len(),
        });
    }
    // Address is in the last 20 bytes of the 32-byte word
    Ok(Address::from_slice(&data[offset + 12..offset + 32]))
}

/// Read a 32-byte word as U256.
fn read_u256(data: &[u8], offset: usize) -> Result<U256, PrecompileInputError> {
    if data.len() < offset + 32 {
        return Err(PrecompileInputError::InputTooShort {
            expected: offset + 32,
            actual: data.len(),
        });
    }
    Ok(U256::from_be_slice(&data[offset..offset + 32]))
}

/// Read a single byte from a 32-byte word (last byte).
fn read_u8(data: &[u8], offset: usize) -> Result<u8, PrecompileInputError> {
    if data.len() < offset + 32 {
        return Err(PrecompileInputError::InputTooShort {
            expected: offset + 32,
            actual: data.len(),
        });
    }
    Ok(data[offset + 31])
}

/// Read a boolean from a 32-byte word.
fn read_bool(data: &[u8], offset: usize) -> Result<bool, PrecompileInputError> {
    let val = read_u8(data, offset)?;
    Ok(val != 0)
}

/// Convert a u8 to SlotType.
fn u8_to_slot_type(val: u8) -> Result<SlotType, PrecompileInputError> {
    match val {
        0 => Ok(SlotType::Simple),
        1 => Ok(SlotType::Mapping),
        2 => Ok(SlotType::NestedMapping),
        3 => Ok(SlotType::MappingToStruct { struct_size: 1 }), // Default struct size
        _ => Err(PrecompileInputError::InvalidSlotType(val)),
    }
}

/// Convert a u8 to OwnershipType.
fn u8_to_ownership_type(val: u8, _data: &[u8], _offset: usize) -> Result<OwnershipType, PrecompileInputError> {
    match val {
        0 => Ok(OwnershipType::Contract),
        1 => Ok(OwnershipType::MappingKey),
        2 => Ok(OwnershipType::OuterKey),
        3 => Ok(OwnershipType::InnerKey),
        4 => {
            // FixedOwner requires reading the next 32 bytes as an address
            // For now, we don't support FixedOwner via precompile (would need extended encoding)
            // This is a simplification - real implementation would read the address
            Err(PrecompileInputError::InvalidOwnershipType(val))
        }
        _ => Err(PrecompileInputError::InvalidOwnershipType(val)),
    }
}

/// Decode a dynamic array of slot configs.
///
/// ABI encoding for dynamic arrays:
/// - First word is the offset to the array data
/// - At that offset: first word is length, followed by elements
///
/// For simplicity, we use a packed encoding here:
/// - Word 0: number of slots
/// - For each slot: 3 words (base_slot, slot_type, ownership)
fn decode_slot_configs(data: &[u8], start_offset: usize) -> Result<Vec<SlotConfigInput>, PrecompileInputError> {
    // Read number of slots from the first word after start_offset
    let num_slots = read_u256(data, start_offset)?;
    let num_slots: usize = num_slots.try_into().map_err(|_| PrecompileInputError::InvalidArrayLength)?;

    // Sanity check to prevent OOM
    if num_slots > 100 {
        return Err(PrecompileInputError::InvalidArrayLength);
    }

    let mut slots = Vec::with_capacity(num_slots);
    let mut offset = start_offset + 32; // Skip the length word

    for _ in 0..num_slots {
        let base_slot = read_u256(data, offset)?;
        offset += 32;

        let slot_type_val = read_u8(data, offset)?;
        offset += 32;

        let ownership_val = read_u8(data, offset)?;
        offset += 32;

        let slot_type = u8_to_slot_type(slot_type_val)?;
        let ownership = u8_to_ownership_type(ownership_val, data, offset)?;

        slots.push(SlotConfigInput {
            base_slot,
            slot_type,
            ownership,
        });
    }

    Ok(slots)
}

// ============================================================================
// Public Decoding Functions
// ============================================================================

/// Decode a registry precompile call.
pub(super) fn decode_registry_call(input: &[u8]) -> Result<RegistryCall, PrecompileInputError> {
    if input.len() < 4 {
        return Err(PrecompileInputError::InputTooShort {
            expected: 4,
            actual: input.len(),
        });
    }

    let selector: [u8; 4] = input[..4].try_into().unwrap();
    let data = &input[4..];

    match selector {
        REGISTER_SELECTOR => {
            // register(address admin, tuple[] slots, bool hideEvents)
            // Simplified encoding: admin (32) + hideEvents (32) + slots array
            let admin = read_address(data, 0)?;
            if admin.is_zero() {
                return Err(PrecompileInputError::ZeroAddress);
            }

            // For simplicity, we expect:
            // - Word 0: admin address
            // - Word 1: hideEvents bool
            // - Word 2: number of slots
            // - Words 3+: slot data (3 words per slot)
            let hide_events = read_bool(data, 32)?;
            let slots = decode_slot_configs(data, 64)?;

            Ok(RegistryCall::Register(RegistrationInput {
                admin,
                slots,
                hide_events,
            }))
        }
        ADD_SLOTS_SELECTOR => {
            // addSlots(tuple[] slots)
            let slots = decode_slot_configs(data, 0)?;
            Ok(RegistryCall::AddSlots(AddSlotsInput { slots }))
        }
        SET_HIDE_EVENTS_SELECTOR => {
            // setHideEvents(bool hide)
            let hide = read_bool(data, 0)?;
            Ok(RegistryCall::SetHideEvents(SetHideEventsInput { hide }))
        }
        IS_REGISTERED_SELECTOR => {
            // isRegistered(address contract)
            let contract = read_address(data, 0)?;
            Ok(RegistryCall::IsRegistered(IsRegisteredInput { contract }))
        }
        _ => Err(PrecompileInputError::UnknownSelector(selector)),
    }
}

/// Decode an auth precompile call.
pub(super) fn decode_auth_call(input: &[u8]) -> Result<AuthCall, PrecompileInputError> {
    if input.len() < 4 {
        return Err(PrecompileInputError::InputTooShort {
            expected: 4,
            actual: input.len(),
        });
    }

    let selector: [u8; 4] = input[..4].try_into().unwrap();
    let data = &input[4..];

    match selector {
        GRANT_SELECTOR => {
            // grant(address contract, uint256 slot, address grantee, uint8 permission)
            let contract = read_address(data, 0)?;
            let slot = read_u256(data, 32)?;
            let grantee = read_address(data, 64)?;
            let permission = read_u8(data, 96)?;

            // Validate permission (1 = READ, 2 = WRITE, 3 = READ|WRITE)
            if permission == 0 || permission > 3 {
                return Err(PrecompileInputError::InvalidPermission(permission));
            }

            Ok(AuthCall::Grant(AuthGrantInput {
                contract,
                slot,
                grantee,
                permission,
            }))
        }
        REVOKE_SELECTOR => {
            // revoke(address contract, uint256 slot, address grantee)
            let contract = read_address(data, 0)?;
            let slot = read_u256(data, 32)?;
            let grantee = read_address(data, 64)?;

            Ok(AuthCall::Revoke(AuthRevokeInput {
                contract,
                slot,
                grantee,
            }))
        }
        IS_AUTHORIZED_SELECTOR => {
            // isAuthorized(address contract, uint256 slot, address query)
            let contract = read_address(data, 0)?;
            let slot = read_u256(data, 32)?;
            let query = read_address(data, 64)?;

            Ok(AuthCall::IsAuthorized(IsAuthorizedInput {
                contract,
                slot,
                query,
            }))
        }
        _ => Err(PrecompileInputError::UnknownSelector(selector)),
    }
}

// ============================================================================
// Encoding Helpers (for building test inputs)
// ============================================================================

/// Encode an address as a 32-byte word.
#[cfg(test)]
pub(crate) fn encode_address(addr: Address) -> [u8; 32] {
    let mut result = [0u8; 32];
    result[12..32].copy_from_slice(addr.as_slice());
    result
}

/// Encode a U256 as a 32-byte word.
#[cfg(test)]
pub(crate) fn encode_u256(val: U256) -> [u8; 32] {
    val.to_be_bytes::<32>()
}

/// Encode a u8 as a 32-byte word.
#[cfg(test)]
pub(crate) fn encode_u8(val: u8) -> [u8; 32] {
    let mut result = [0u8; 32];
    result[31] = val;
    result
}

/// Encode a bool as a 32-byte word.
#[cfg(test)]
pub(crate) fn encode_bool(val: bool) -> [u8; 32] {
    encode_u8(if val { 1 } else { 0 })
}

/// Build a registration input for testing.
#[cfg(test)]
pub(crate) fn build_register_input(
    admin: Address,
    hide_events: bool,
    slots: &[SlotConfigInput],
) -> Vec<u8> {
    let mut result = Vec::new();
    result.extend_from_slice(&REGISTER_SELECTOR);
    result.extend_from_slice(&encode_address(admin));
    result.extend_from_slice(&encode_bool(hide_events));
    result.extend_from_slice(&encode_u256(U256::from(slots.len())));

    for slot in slots {
        result.extend_from_slice(&encode_u256(slot.base_slot));
        result.extend_from_slice(&encode_u8(slot.slot_type.to_u8()));
        result.extend_from_slice(&encode_u8(slot.ownership.to_u8()));
    }

    result
}

/// Build a grant input for testing.
#[cfg(test)]
pub(crate) fn build_grant_input(
    contract: Address,
    slot: U256,
    grantee: Address,
    permission: u8,
) -> Vec<u8> {
    let mut result = Vec::new();
    result.extend_from_slice(&GRANT_SELECTOR);
    result.extend_from_slice(&encode_address(contract));
    result.extend_from_slice(&encode_u256(slot));
    result.extend_from_slice(&encode_address(grantee));
    result.extend_from_slice(&encode_u8(permission));
    result
}

/// Build a revoke input for testing.
#[cfg(test)]
pub(crate) fn build_revoke_input(contract: Address, slot: U256, grantee: Address) -> Vec<u8> {
    let mut result = Vec::new();
    result.extend_from_slice(&REVOKE_SELECTOR);
    result.extend_from_slice(&encode_address(contract));
    result.extend_from_slice(&encode_u256(slot));
    result.extend_from_slice(&encode_address(grantee));
    result
}

/// Build an isAuthorized input for testing.
#[cfg(test)]
pub(crate) fn build_is_authorized_input(contract: Address, slot: U256, query: Address) -> Vec<u8> {
    let mut result = Vec::new();
    result.extend_from_slice(&IS_AUTHORIZED_SELECTOR);
    result.extend_from_slice(&encode_address(contract));
    result.extend_from_slice(&encode_u256(slot));
    result.extend_from_slice(&encode_address(query));
    result
}

/// Build an isRegistered input for testing.
#[cfg(test)]
pub(crate) fn build_is_registered_input(contract: Address) -> Vec<u8> {
    let mut result = Vec::new();
    result.extend_from_slice(&IS_REGISTERED_SELECTOR);
    result.extend_from_slice(&encode_address(contract));
    result
}

// ============================================================================
// SlotType and OwnershipType helpers
// ============================================================================

#[cfg(test)]
impl SlotType {
    /// Convert SlotType to u8 for encoding.
    pub(crate) fn to_u8(&self) -> u8 {
        match self {
            SlotType::Simple => 0,
            SlotType::Mapping => 1,
            SlotType::NestedMapping => 2,
            SlotType::MappingToStruct { .. } => 3,
        }
    }
}

#[cfg(test)]
impl OwnershipType {
    /// Convert OwnershipType to u8 for encoding.
    pub(crate) fn to_u8(&self) -> u8 {
        match self {
            OwnershipType::Contract => 0,
            OwnershipType::MappingKey => 1,
            OwnershipType::OuterKey => 2,
            OwnershipType::InnerKey => 3,
            OwnershipType::FixedOwner(_) => 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_address(n: u8) -> Address {
        Address::new([n; 20])
    }

    #[test]
    fn test_read_address() {
        let mut data = [0u8; 32];
        let addr = test_address(0xAB);
        data[12..32].copy_from_slice(addr.as_slice());

        let result = read_address(&data, 0).unwrap();
        assert_eq!(result, addr);
    }

    #[test]
    fn test_read_u256() {
        let mut data = [0u8; 32];
        data[31] = 42;

        let result = read_u256(&data, 0).unwrap();
        assert_eq!(result, U256::from(42));
    }

    #[test]
    fn test_read_bool() {
        let mut data = [0u8; 32];
        data[31] = 1;
        assert!(read_bool(&data, 0).unwrap());

        data[31] = 0;
        assert!(!read_bool(&data, 0).unwrap());

        data[31] = 255;
        assert!(read_bool(&data, 0).unwrap());
    }

    #[test]
    fn test_decode_register_empty_slots() {
        let admin = test_address(1);
        let input = build_register_input(admin, false, &[]);

        let call = decode_registry_call(&input).unwrap();
        match call {
            RegistryCall::Register(reg) => {
                assert_eq!(reg.admin, admin);
                assert!(!reg.hide_events);
                assert!(reg.slots.is_empty());
            }
            _ => panic!("Expected Register call"),
        }
    }

    #[test]
    fn test_decode_register_with_slots() {
        let admin = test_address(1);
        let slots = vec![
            SlotConfigInput {
                base_slot: U256::from(5),
                slot_type: SlotType::Simple,
                ownership: OwnershipType::Contract,
            },
            SlotConfigInput {
                base_slot: U256::from(10),
                slot_type: SlotType::Mapping,
                ownership: OwnershipType::MappingKey,
            },
        ];
        let input = build_register_input(admin, true, &slots);

        let call = decode_registry_call(&input).unwrap();
        match call {
            RegistryCall::Register(reg) => {
                assert_eq!(reg.admin, admin);
                assert!(reg.hide_events);
                assert_eq!(reg.slots.len(), 2);
                assert_eq!(reg.slots[0].base_slot, U256::from(5));
                assert_eq!(reg.slots[0].slot_type, SlotType::Simple);
                assert_eq!(reg.slots[1].slot_type, SlotType::Mapping);
            }
            _ => panic!("Expected Register call"),
        }
    }

    #[test]
    fn test_decode_is_registered() {
        let contract = test_address(42);
        let input = build_is_registered_input(contract);

        let call = decode_registry_call(&input).unwrap();
        match call {
            RegistryCall::IsRegistered(q) => {
                assert_eq!(q.contract, contract);
            }
            _ => panic!("Expected IsRegistered call"),
        }
    }

    #[test]
    fn test_decode_grant() {
        let contract = test_address(1);
        let slot = U256::from(100);
        let grantee = test_address(2);
        let permission = 1; // READ

        let input = build_grant_input(contract, slot, grantee, permission);

        let call = decode_auth_call(&input).unwrap();
        match call {
            AuthCall::Grant(g) => {
                assert_eq!(g.contract, contract);
                assert_eq!(g.slot, slot);
                assert_eq!(g.grantee, grantee);
                assert_eq!(g.permission, 1);
            }
            _ => panic!("Expected Grant call"),
        }
    }

    #[test]
    fn test_decode_revoke() {
        let contract = test_address(1);
        let slot = U256::from(50);
        let grantee = test_address(3);

        let input = build_revoke_input(contract, slot, grantee);

        let call = decode_auth_call(&input).unwrap();
        match call {
            AuthCall::Revoke(r) => {
                assert_eq!(r.contract, contract);
                assert_eq!(r.slot, slot);
                assert_eq!(r.grantee, grantee);
            }
            _ => panic!("Expected Revoke call"),
        }
    }

    #[test]
    fn test_decode_is_authorized() {
        let contract = test_address(1);
        let slot = U256::from(200);
        let query = test_address(4);

        let input = build_is_authorized_input(contract, slot, query);

        let call = decode_auth_call(&input).unwrap();
        match call {
            AuthCall::IsAuthorized(a) => {
                assert_eq!(a.contract, contract);
                assert_eq!(a.slot, slot);
                assert_eq!(a.query, query);
            }
            _ => panic!("Expected IsAuthorized call"),
        }
    }

    #[test]
    fn test_unknown_selector() {
        let input = [0xde, 0xad, 0xbe, 0xef, 0x00];
        let result = decode_registry_call(&input);
        assert!(matches!(
            result,
            Err(PrecompileInputError::UnknownSelector([0xde, 0xad, 0xbe, 0xef]))
        ));
    }

    #[test]
    fn test_input_too_short() {
        let input = [0x08]; // Just one byte
        let result = decode_registry_call(&input);
        assert!(matches!(result, Err(PrecompileInputError::InputTooShort { .. })));
    }

    #[test]
    fn test_zero_admin_address() {
        let input = build_register_input(Address::ZERO, false, &[]);
        let result = decode_registry_call(&input);
        assert!(matches!(result, Err(PrecompileInputError::ZeroAddress)));
    }

    #[test]
    fn test_invalid_permission() {
        let contract = test_address(1);
        let slot = U256::from(100);
        let grantee = test_address(2);

        // Permission 0 is invalid
        let input = build_grant_input(contract, slot, grantee, 0);
        let result = decode_auth_call(&input);
        assert!(matches!(result, Err(PrecompileInputError::InvalidPermission(0))));

        // Permission 4 is invalid
        let input = build_grant_input(contract, slot, grantee, 4);
        let result = decode_auth_call(&input);
        assert!(matches!(result, Err(PrecompileInputError::InvalidPermission(4))));
    }

    #[test]
    fn test_invalid_slot_type() {
        // Build input manually with invalid slot type (99)
        let mut input = Vec::new();
        input.extend_from_slice(&REGISTER_SELECTOR);
        input.extend_from_slice(&encode_address(test_address(1)));
        input.extend_from_slice(&encode_bool(false));
        input.extend_from_slice(&encode_u256(U256::from(1))); // 1 slot
        input.extend_from_slice(&encode_u256(U256::from(5))); // base_slot
        input.extend_from_slice(&encode_u8(99)); // invalid slot_type
        input.extend_from_slice(&encode_u8(0)); // ownership

        let result = decode_registry_call(&input);
        assert!(matches!(result, Err(PrecompileInputError::InvalidSlotType(99))));
    }

    #[test]
    fn test_slot_type_round_trip() {
        assert_eq!(u8_to_slot_type(0).unwrap(), SlotType::Simple);
        assert_eq!(u8_to_slot_type(1).unwrap(), SlotType::Mapping);
        assert_eq!(u8_to_slot_type(2).unwrap(), SlotType::NestedMapping);

        assert_eq!(SlotType::Simple.to_u8(), 0);
        assert_eq!(SlotType::Mapping.to_u8(), 1);
        assert_eq!(SlotType::NestedMapping.to_u8(), 2);
    }

    #[test]
    fn test_ownership_type_round_trip() {
        // We need a dummy buffer for the function signature, but these types don't need it
        let dummy = [0u8; 64];

        assert_eq!(u8_to_ownership_type(0, &dummy, 0).unwrap(), OwnershipType::Contract);
        assert_eq!(u8_to_ownership_type(1, &dummy, 0).unwrap(), OwnershipType::MappingKey);
        assert_eq!(u8_to_ownership_type(2, &dummy, 0).unwrap(), OwnershipType::OuterKey);
        assert_eq!(u8_to_ownership_type(3, &dummy, 0).unwrap(), OwnershipType::InnerKey);

        assert_eq!(OwnershipType::Contract.to_u8(), 0);
        assert_eq!(OwnershipType::MappingKey.to_u8(), 1);
        assert_eq!(OwnershipType::OuterKey.to_u8(), 2);
        assert_eq!(OwnershipType::InnerKey.to_u8(), 3);
    }
}
