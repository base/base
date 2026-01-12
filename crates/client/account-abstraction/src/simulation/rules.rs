//! ERC-7562 Rule Checking
//!
//! This module implements the ERC-7562 validation scope rules for Account Abstraction.
//! It checks opcode usage, storage access patterns, and other rules against traced execution.
//!
//! Reference: https://eips.ethereum.org/EIPS/eip-7562

use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_trace::geth::erc7562::Erc7562Frame;

use super::entity::is_entry_point;
use super::types::{Entities, EntitiesInfo, Erc7562Rule, RuleViolation, StakeInfo};

/// Opcodes banned during validation (OP-011)
pub const BANNED_OPCODES: &[u8] = &[
    0x31, // BALANCE
    0x32, // ORIGIN
    0x3A, // GASPRICE
    0x40, // BLOCKHASH
    0x41, // COINBASE
    0x42, // TIMESTAMP
    0x43, // NUMBER
    0x44, // PREVRANDAO/DIFFICULTY
    0x45, // GASLIMIT
    0x47, // SELFBALANCE
    0x48, // BASEFEE
    0x5A, // GAS (unless followed by *CALL - checked separately)
    0xF0, // CREATE
    0xFE, // INVALID
    0xFF, // SELFDESTRUCT
];

/// Opcodes that are CALL variants (used for GAS opcode check)
pub const CALL_OPCODES: &[u8] = &[
    0xF1, // CALL
    0xF2, // CALLCODE
    0xF4, // DELEGATECALL
    0xFA, // STATICCALL
];

/// Known precompiles that are allowed (OP-062)
/// Core precompiles 0x01 - 0x09 plus RIP-7212 secp256r1
pub const ALLOWED_PRECOMPILES: &[Address] = &[
    // Core precompiles 0x01 - 0x09
    Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01]), // ecrecover
    Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02]), // sha256
    Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x03]), // ripemd160
    Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x04]), // identity
    Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x05]), // modexp
    Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x06]), // ecadd
    Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x07]), // ecmul
    Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x08]), // ecpairing
    Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x09]), // blake2f
    // RIP-7212 secp256r1 precompile
    Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0x00]),
];

/// CREATE2 opcode
pub const CREATE2_OPCODE: u8 = 0xF5;

/// Check if an address is a precompile (0x01 - 0x09 range or RIP-7212)
pub fn is_precompile(address: Address) -> bool {
    let bytes = address.as_slice();
    // Check if first 18 bytes are zero
    if bytes[..18].iter().all(|&b| b == 0) {
        // Standard precompiles: 0x01 - 0x09
        if bytes[18] == 0 && bytes[19] >= 0x01 && bytes[19] <= 0x09 {
            return true;
        }
        // RIP-7212 precompile: 0x0100
        if bytes[18] == 0x01 && bytes[19] == 0x00 {
            return true;
        }
    }
    false
}

/// Check if a precompile is in the allowed list
pub fn is_allowed_precompile(address: Address) -> bool {
    ALLOWED_PRECOMPILES.contains(&address)
}

/// Get opcode name for display
pub fn opcode_name(opcode: u8) -> &'static str {
    match opcode {
        0x31 => "BALANCE",
        0x32 => "ORIGIN",
        0x3A => "GASPRICE",
        0x40 => "BLOCKHASH",
        0x41 => "COINBASE",
        0x42 => "TIMESTAMP",
        0x43 => "NUMBER",
        0x44 => "PREVRANDAO",
        0x45 => "GASLIMIT",
        0x47 => "SELFBALANCE",
        0x48 => "BASEFEE",
        0x5A => "GAS",
        0xF0 => "CREATE",
        0xF5 => "CREATE2",
        0xFE => "INVALID",
        0xFF => "SELFDESTRUCT",
        0xF1 => "CALL",
        0xF2 => "CALLCODE",
        0xF4 => "DELEGATECALL",
        0xFA => "STATICCALL",
        0x3B => "EXTCODESIZE",
        0x3C => "EXTCODECOPY",
        0x3F => "EXTCODEHASH",
        0x54 => "SLOAD",
        0x55 => "SSTORE",
        0x5C => "TLOAD",
        0x5D => "TSTORE",
        _ => "UNKNOWN",
    }
}

/// ERC-7562 rule checker
pub struct Erc7562RuleChecker {
    /// Whether the account already exists on-chain
    pub account_exists: bool,
}

impl Default for Erc7562RuleChecker {
    fn default() -> Self {
        Self::new()
    }
}

impl Erc7562RuleChecker {
    /// Create a new rule checker
    pub fn new() -> Self {
        Self {
            account_exists: false,
        }
    }

    /// Create a rule checker with account existence flag
    pub fn with_account_exists(account_exists: bool) -> Self {
        Self { account_exists }
    }

    /// Check all ERC-7562 rules against a traced execution
    pub fn check_all_rules(
        &self,
        trace: &Erc7562Frame,
        entities: &Entities,
        entities_info: &EntitiesInfo,
    ) -> Vec<RuleViolation> {
        let mut violations = Vec::new();

        // Recursively check each frame
        self.check_frame(trace, entities, entities_info, &mut violations, 0);

        violations
    }

    /// Check a single frame and its children
    fn check_frame(
        &self,
        frame: &Erc7562Frame,
        entities: &Entities,
        entities_info: &EntitiesInfo,
        violations: &mut Vec<RuleViolation>,
        depth: usize,
    ) {
        let current_entity = frame.from;

        // OP-011: Check banned opcodes
        self.check_banned_opcodes(frame, current_entity, violations);

        // OP-020: Check out of gas
        if frame.out_of_gas {
            violations.push(RuleViolation::out_of_gas(current_entity));
        }

        // OP-031: Check CREATE2 restrictions
        self.check_create2_rules(frame, entities, violations);

        // OP-041: Check EXTCODE* access on addresses without code
        self.check_ext_code_access(frame, entities, violations);

        // OP-05x: Check EntryPoint access restrictions
        self.check_entrypoint_access(frame, entities, violations);

        // OP-061: Check CALL with value
        self.check_call_with_value(frame, violations);

        // OP-062: Check precompile access
        self.check_precompile_access(frame, violations);

        // STO-*: Check storage access rules
        self.check_storage_rules(frame, entities, entities_info, violations);

        // Note: AUTH-020, AUTH-030 (EIP-7702 delegation rules) are checked in
        // UserOperationValidator::check_eip7702_delegations() using eth_getCode
        // to detect the 0xef0100 delegation prefix.
        //
        // AUTH-010 (single authorization tuple) is checked at the RPC layer.
        // AUTH-040 (same delegate for multiple UserOps) is a local/mempool rule.

        // Recursively check child frames
        for child in &frame.calls {
            self.check_frame(child, entities, entities_info, violations, depth + 1);
        }
    }

    /// Check for banned opcodes (OP-011)
    fn check_banned_opcodes(
        &self,
        frame: &Erc7562Frame,
        entity: Address,
        violations: &mut Vec<RuleViolation>,
    ) {
        for (&opcode, &count) in &frame.used_opcodes {
            if count > 0 && BANNED_OPCODES.contains(&opcode) {
                // Special case: GAS (0x5A) is allowed if followed by *CALL
                // The tracer doesn't give us this info directly, so we allow GAS
                // if there are also CALL opcodes in the frame
                if opcode == 0x5A {
                    let has_call = frame.used_opcodes.iter().any(|(&op, &c)| {
                        c > 0 && CALL_OPCODES.contains(&op)
                    });
                    if has_call {
                        continue; // GAS followed by CALL is allowed
                    }
                }

                violations.push(RuleViolation::banned_opcode(
                    entity,
                    opcode,
                    opcode_name(opcode),
                ));
            }
        }
    }

    /// Check CREATE2 restrictions (OP-031)
    ///
    /// CREATE2 is allowed only:
    /// - By the factory during account deployment (to create the sender)
    /// - Only once per validation
    fn check_create2_rules(
        &self,
        frame: &Erc7562Frame,
        entities: &Entities,
        violations: &mut Vec<RuleViolation>,
    ) {
        // Check if CREATE2 was used
        let create2_count = frame
            .used_opcodes
            .get(&CREATE2_OPCODE)
            .copied()
            .unwrap_or(0);

        if create2_count == 0 {
            return;
        }

        let caller = frame.from;

        // CREATE2 is only allowed by the factory
        if entities.factory != Some(caller) {
            violations.push(RuleViolation::invalid_create2(
                caller,
                create2_count as usize,
                format!(
                    "CREATE2 used by non-factory entity (caller: {}, factory: {:?})",
                    caller, entities.factory
                ),
            ));
            return;
        }

        // CREATE2 should only be used once (to create the sender)
        if create2_count > 1 {
            violations.push(RuleViolation::invalid_create2(
                caller,
                create2_count as usize,
                format!("CREATE2 used {} times (only 1 allowed)", create2_count),
            ));
        }
    }

    /// Check EXTCODE* access rules (OP-041, OP-042)
    fn check_ext_code_access(
        &self,
        frame: &Erc7562Frame,
        entities: &Entities,
        violations: &mut Vec<RuleViolation>,
    ) {
        for addr_str in &frame.ext_code_access_info {
            // Parse the address from string (format: "0x...")
            if let Ok(address) = addr_str.parse::<Address>() {
                // Check if the address has code
                if let Some(contract_info) = frame.contract_size.get(&address) {
                    if contract_info.contract_size == 0 {
                        // OP-042: Exception - sender access is allowed in factory
                        if address == entities.sender {
                            continue;
                        }

                        // OP-041: EXTCODE* on address without code
                        violations.push(RuleViolation::ext_code_access(frame.from, address));
                    }
                }
            }
        }
    }

    /// Check EntryPoint access restrictions (OP-051 to OP-054)
    ///
    /// Rules:
    /// - OP-051: Only depositTo on EntryPoint allowed during validation
    /// - OP-052: Cannot call EntryPoint recursively during inner call
    /// - OP-053: EntryPoint code cannot be accessed (via EXTCODE*)
    /// - OP-054: EntryPoint state cannot be accessed (via SLOAD/SSTORE)
    fn check_entrypoint_access(
        &self,
        frame: &Erc7562Frame,
        _entities: &Entities,
        violations: &mut Vec<RuleViolation>,
    ) {
        // Check if this frame is a call to the EntryPoint
        if let Some(to) = frame.to {
            if is_entry_point(to) {
                // OP-052: Recursive calls to EntryPoint are forbidden
                // (This is a nested call to EntryPoint within validation)
                violations.push(RuleViolation::invalid_entrypoint_access(
                    frame.from,
                    "Recursive call to EntryPoint during validation (OP-052)",
                ));
            }
        }

        // OP-053: Check if EntryPoint code was accessed via EXTCODE*
        for addr_str in &frame.ext_code_access_info {
            if let Ok(address) = addr_str.parse::<Address>() {
                if is_entry_point(address) {
                    violations.push(RuleViolation::invalid_entrypoint_access(
                        frame.from,
                        format!(
                            "EXTCODE* on EntryPoint {} not allowed (OP-053)",
                            address
                        ),
                    ));
                }
            }
        }
    }

    /// Check CALL with value restrictions (OP-061)
    ///
    /// CALL with value is only allowed to EntryPoint.depositTo
    /// The tracer doesn't give us the exact method being called, so we check
    /// if there's any CALL with value to a non-EntryPoint address.
    fn check_call_with_value(
        &self,
        frame: &Erc7562Frame,
        violations: &mut Vec<RuleViolation>,
    ) {
        // Check child calls for value transfers
        for child in &frame.calls {
            if let Some(to) = child.to {
                // If there's value being transferred (value is Option<U256>)
                if let Some(value) = child.value {
                    if value > U256::ZERO {
                        // Value transfer to EntryPoint is allowed (depositTo)
                        if !is_entry_point(to) {
                            violations.push(RuleViolation::call_with_value(
                                frame.from,
                                to,
                                value,
                            ));
                        }
                    }
                }
            }
        }
    }

    /// Check precompile access restrictions (OP-062)
    ///
    /// Only specific precompiles are allowed during validation.
    fn check_precompile_access(
        &self,
        frame: &Erc7562Frame,
        violations: &mut Vec<RuleViolation>,
    ) {
        // Check child calls for precompile access
        for child in &frame.calls {
            if let Some(to) = child.to {
                // Check if this is a precompile address
                if is_precompile(to) && !is_allowed_precompile(to) {
                    violations.push(RuleViolation::invalid_precompile(frame.from, to));
                }
            }
        }
    }

    /// Check storage access rules (STO-*)
    ///
    /// Storage access rules from ERC-7562:
    /// - STO-010: Account storage (sender's own storage) is always allowed
    /// - STO-021: Associated storage of sender in external contract (needs account to exist OR staked factory)
    /// - STO-022: If account doesn't exist and no factory, associated storage needs staked factory
    /// - STO-031: Entity storage (paymaster/factory's own storage) needs stake for writes
    /// - STO-032: Associated storage in non-entity contract needs stake
    /// - STO-033: Read-only access to non-entity storage needs stake
    fn check_storage_rules(
        &self,
        frame: &Erc7562Frame,
        entities: &Entities,
        entities_info: &EntitiesInfo,
        violations: &mut Vec<RuleViolation>,
    ) {
        let accessor = frame.from;
        let is_entity = entities.is_entity(accessor);

        // Get stake info for current accessor
        let accessor_stake_info = self.get_stake_info_for_address(accessor, entities, entities_info);

        // The frame's accessed_slots contains the storage address being accessed
        // We need to determine WHICH contract's storage is being accessed
        // In the ERC-7562 tracer, the `from` field indicates who is accessing,
        // and the storage slots are from that contract's context

        // Check read slots
        for (slot, _values) in &frame.accessed_slots.reads {
            self.check_storage_slot_access(
                *slot,
                false, // is_write
                accessor,
                is_entity,
                &accessor_stake_info,
                entities,
                entities_info,
                violations,
            );
        }

        // Check write slots
        for (slot, _count) in &frame.accessed_slots.writes {
            self.check_storage_slot_access(
                *slot,
                true, // is_write
                accessor,
                is_entity,
                &accessor_stake_info,
                entities,
                entities_info,
                violations,
            );
        }

        // OP-070: Transient storage follows same rules as persistent storage
        for (slot, _count) in &frame.accessed_slots.transient_reads {
            self.check_storage_slot_access(
                *slot,
                false,
                accessor,
                is_entity,
                &accessor_stake_info,
                entities,
                entities_info,
                violations,
            );
        }

        for (slot, _count) in &frame.accessed_slots.transient_writes {
            self.check_storage_slot_access(
                *slot,
                true,
                accessor,
                is_entity,
                &accessor_stake_info,
                entities,
                entities_info,
                violations,
            );
        }
    }

    /// Check a single storage slot access
    ///
    /// # Arguments
    /// * `slot` - The storage slot being accessed
    /// * `is_write` - Whether this is a write (SSTORE) vs read (SLOAD)
    /// * `accessor` - The address accessing the storage (the contract executing)
    /// * `accessor_is_entity` - Whether the accessor is an entity (sender/factory/paymaster/aggregator)
    /// * `accessor_stake_info` - Stake info for the accessor if it's an entity
    /// * `entities` - All entities involved in this UserOp
    /// * `entities_info` - Stake info for all entities
    /// * `violations` - Vector to collect violations
    #[allow(clippy::too_many_arguments)]
    fn check_storage_slot_access(
        &self,
        slot: B256,
        is_write: bool,
        accessor: Address,
        accessor_is_entity: bool,
        accessor_stake_info: &Option<StakeInfo>,
        entities: &Entities,
        entities_info: &EntitiesInfo,
        violations: &mut Vec<RuleViolation>,
    ) {
        // STO-010: Sender's own storage is always allowed
        if accessor == entities.sender {
            return;
        }

        // Check if accessor is staked
        let is_staked = accessor_stake_info
            .as_ref()
            .map(|s| s.is_staked)
            .unwrap_or(false);

        // Get factory stake status (for STO-022)
        let factory_is_staked = entities.factory.map_or(false, |_| {
            entities_info
                .factory
                .as_ref()
                .map(|f| f.is_staked)
                .unwrap_or(false)
        });

        // STO-031: Entity storage access
        if accessor_is_entity {
            if is_write && !is_staked {
                // Entity writing to its own storage needs stake
                violations.push(RuleViolation::storage_access(
                    Erc7562Rule::Sto031EntityStorageNeedsStake,
                    accessor,
                    slot,
                    "Entity storage write requires stake (STO-031)",
                ));
            }
            return;
        }

        // Non-entity storage access (accessor is a library/helper contract)
        // This is more restrictive

        // STO-021/022: Associated storage rules
        if !self.account_exists {
            // Account doesn't exist yet - this is a deployment
            if entities.factory.is_none() {
                // No factory means we can't create the account - invalid UserOp
                violations.push(RuleViolation::storage_access(
                    Erc7562Rule::Sto022AssociatedStorageNeedsStakedFactory,
                    accessor,
                    slot,
                    "Storage access without factory when account doesn't exist (STO-022)",
                ));
            } else if !factory_is_staked {
                // Has factory but it's not staked
                violations.push(RuleViolation::storage_access(
                    Erc7562Rule::Sto022AssociatedStorageNeedsStakedFactory,
                    accessor,
                    slot,
                    "Storage access requires staked factory when account doesn't exist (STO-022)",
                ));
            }
        }

        // STO-032: Associated storage in non-entity contract needs stake
        // STO-033: Read-only access to non-entity storage also needs stake
        if !is_staked {
            if is_write {
                violations.push(RuleViolation::storage_access(
                    Erc7562Rule::Sto032AssociatedNonEntityNeedsStake,
                    accessor,
                    slot,
                    "Non-entity storage write requires stake (STO-032)",
                ));
            } else {
                violations.push(RuleViolation::storage_access(
                    Erc7562Rule::Sto033ReadOnlyNonEntityNeedsStake,
                    accessor,
                    slot,
                    "Non-entity storage read requires stake (STO-033)",
                ));
            }
        }
    }

    /// Get stake info for an address from entities info
    fn get_stake_info_for_address(
        &self,
        address: Address,
        entities: &Entities,
        entities_info: &EntitiesInfo,
    ) -> Option<StakeInfo> {
        if address == entities.sender {
            Some(entities_info.sender.clone())
        } else if entities.factory == Some(address) {
            entities_info.factory.clone()
        } else if entities.paymaster == Some(address) {
            entities_info.paymaster.clone()
        } else if entities.aggregator == Some(address) {
            entities_info.aggregator.as_ref().map(|a| a.stake_info.clone())
        } else {
            None
        }
    }

    /// Check if a slot is "associated" with an address
    /// A slot is associated with address A if:
    /// 1. The slot value is A
    /// 2. The slot was calculated as keccak(A||x)+n where n is in range 0..128
    pub fn is_associated_slot(slot: B256, address: Address, keccak_preimages: &[&[u8]]) -> bool {
        // Check if slot value equals address (padded to 32 bytes)
        let mut address_padded = [0u8; 32];
        address_padded[12..32].copy_from_slice(address.as_slice());
        if slot.as_slice() == address_padded {
            return true;
        }

        // Check keccak preimages for association
        for preimage in keccak_preimages {
            if preimage.len() >= 20 {
                // Check if preimage starts with address
                let potential_address = Address::from_slice(&preimage[..20]);
                if potential_address == address {
                    // Calculate expected slot and check if within range
                    use alloy_primitives::keccak256;
                    let base_slot = keccak256(preimage);
                    let slot_u256 = U256::from_be_bytes(*slot.as_ref());
                    let base_u256 = U256::from_be_bytes(*base_slot.as_ref());

                    if slot_u256 >= base_u256 && slot_u256 - base_u256 <= U256::from(128) {
                        return true;
                    }
                }
            }
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============================================================
    // Basic Tests - Helper Functions and Constants
    // ============================================================

    #[test]
    fn test_opcode_name() {
        assert_eq!(opcode_name(0x31), "BALANCE");
        assert_eq!(opcode_name(0x5A), "GAS");
        assert_eq!(opcode_name(0xF5), "CREATE2");
        assert_eq!(opcode_name(0xFF), "SELFDESTRUCT");
    }

    #[test]
    fn test_opcode_name_call_variants() {
        assert_eq!(opcode_name(0xF1), "CALL");
        assert_eq!(opcode_name(0xF2), "CALLCODE");
        assert_eq!(opcode_name(0xF4), "DELEGATECALL");
        assert_eq!(opcode_name(0xFA), "STATICCALL");
    }

    #[test]
    fn test_opcode_name_storage() {
        assert_eq!(opcode_name(0x54), "SLOAD");
        assert_eq!(opcode_name(0x55), "SSTORE");
        assert_eq!(opcode_name(0x5C), "TLOAD");
        assert_eq!(opcode_name(0x5D), "TSTORE");
    }

    #[test]
    fn test_opcode_name_extcode() {
        assert_eq!(opcode_name(0x3B), "EXTCODESIZE");
        assert_eq!(opcode_name(0x3C), "EXTCODECOPY");
        assert_eq!(opcode_name(0x3F), "EXTCODEHASH");
    }

    #[test]
    fn test_opcode_name_unknown() {
        assert_eq!(opcode_name(0x00), "UNKNOWN");
        assert_eq!(opcode_name(0x99), "UNKNOWN");
    }

    #[test]
    fn test_banned_opcodes() {
        assert!(BANNED_OPCODES.contains(&0x31)); // BALANCE
        assert!(BANNED_OPCODES.contains(&0xFF)); // SELFDESTRUCT
        assert!(BANNED_OPCODES.contains(&0xF0)); // CREATE is banned
        assert!(!BANNED_OPCODES.contains(&0xF1)); // CALL is not banned
        assert!(!BANNED_OPCODES.contains(&0xF5)); // CREATE2 is not in banned list (has special rules)
    }

    #[test]
    fn test_banned_opcodes_all() {
        // Verify all banned opcodes
        let expected_banned = vec![
            0x31, // BALANCE
            0x32, // ORIGIN
            0x3A, // GASPRICE
            0x40, // BLOCKHASH
            0x41, // COINBASE
            0x42, // TIMESTAMP
            0x43, // NUMBER
            0x44, // PREVRANDAO/DIFFICULTY
            0x45, // GASLIMIT
            0x47, // SELFBALANCE
            0x48, // BASEFEE
            0x5A, // GAS
            0xF0, // CREATE
            0xFE, // INVALID
            0xFF, // SELFDESTRUCT
        ];

        for opcode in expected_banned {
            assert!(BANNED_OPCODES.contains(&opcode), "Opcode 0x{:02x} should be banned", opcode);
        }
    }

    #[test]
    fn test_call_opcodes() {
        assert!(CALL_OPCODES.contains(&0xF1)); // CALL
        assert!(CALL_OPCODES.contains(&0xF2)); // CALLCODE
        assert!(CALL_OPCODES.contains(&0xF4)); // DELEGATECALL
        assert!(CALL_OPCODES.contains(&0xFA)); // STATICCALL
        assert!(!CALL_OPCODES.contains(&0xF0)); // CREATE is not a CALL
        assert!(!CALL_OPCODES.contains(&0xF5)); // CREATE2 is not a CALL
    }

    #[test]
    fn test_create2_opcode_constant() {
        assert_eq!(CREATE2_OPCODE, 0xF5);
    }

    // ============================================================
    // Precompile Tests
    // ============================================================

    #[test]
    fn test_is_precompile() {
        // Standard precompiles 0x01 - 0x09
        let ecrecover = Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01]);
        assert!(is_precompile(ecrecover));

        let blake2f = Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x09]);
        assert!(is_precompile(blake2f));

        // RIP-7212 precompile
        let secp256r1 = Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0x00]);
        assert!(is_precompile(secp256r1));

        // Not a precompile
        let random = Address::new([0x12, 0x34, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(!is_precompile(random));

        // 0x00 is not a valid precompile
        let zero = Address::ZERO;
        assert!(!is_precompile(zero));

        // 0x0a and above are not precompiles (yet)
        let not_precompile = Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0a]);
        assert!(!is_precompile(not_precompile));
    }

    #[test]
    fn test_is_precompile_all_standard() {
        // Test all standard precompiles 0x01 through 0x09
        for i in 1u8..=9 {
            let addr = Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, i]);
            assert!(is_precompile(addr), "Precompile 0x{:02x} should be recognized", i);
        }
    }

    #[test]
    fn test_allowed_precompiles() {
        let ecrecover = Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01]);
        assert!(is_allowed_precompile(ecrecover));

        let secp256r1 = Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0x00]);
        assert!(is_allowed_precompile(secp256r1));

        // Not in allowed list
        let random = Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0a]);
        assert!(!is_allowed_precompile(random));
    }

    #[test]
    fn test_allowed_precompiles_count() {
        // Should have 10 allowed precompiles (0x01-0x09 + RIP-7212)
        assert_eq!(ALLOWED_PRECOMPILES.len(), 10);
    }

    // ============================================================
    // Rule Checker Initialization Tests
    // ============================================================

    #[test]
    fn test_rule_checker_new() {
        let checker = Erc7562RuleChecker::new();
        assert!(!checker.account_exists);
    }

    #[test]
    fn test_rule_checker_default() {
        let checker = Erc7562RuleChecker::default();
        assert!(!checker.account_exists);
    }

    #[test]
    fn test_rule_checker_with_account_exists_true() {
        let checker = Erc7562RuleChecker::with_account_exists(true);
        assert!(checker.account_exists);
    }

    #[test]
    fn test_rule_checker_with_account_exists_false() {
        let checker = Erc7562RuleChecker::with_account_exists(false);
        assert!(!checker.account_exists);
    }
}

