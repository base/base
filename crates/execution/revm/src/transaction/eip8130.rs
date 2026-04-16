//! EIP-8130 AA transaction data carried through the execution pipeline.
//!
//! These types mirror the subset of [`TxEip8130`] fields the handler needs for
//! phased call execution, auto-delegation, and pre-execution storage writes.
//! They use only primitive types to avoid a circular dependency on
//! `base-alloy-consensus`.

use std::vec::Vec;

use revm::primitives::{Address, B256, Bytes, Log, LogData, U256, keccak256};

/// A single call within an AA transaction phase.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Eip8130Call {
    /// Target address.
    pub to: Address,
    /// Calldata.
    pub data: Bytes,
    /// Value to transfer.
    pub value: U256,
}

/// A pre-execution storage write (nonce increment, owner registration, etc.).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Eip8130StorageWrite {
    /// Contract address holding the storage.
    pub address: Address,
    /// Storage slot key.
    pub slot: U256,
    /// New value to write.
    pub value: U256,
}

/// Code placement for auto-delegation (EIP-7702 style delegation designator).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Eip8130CodePlacement {
    /// Address to place code at.
    pub address: Address,
    /// Bytecode to set.
    pub code: Bytes,
}

/// Per-phase execution result.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Eip8130PhaseResult {
    /// Whether the phase succeeded.
    pub success: bool,
    /// Gas consumed by the phase.
    pub gas_used: u64,
}

/// A config-change sequence update applied as a read-modify-write on the
/// packed `ChangeSequences { uint64 multichain; uint64 local }` slot.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Eip8130SequenceUpdate {
    /// Pre-computed storage slot for `_changeSequences[account]`.
    pub slot: U256,
    /// `true` = update the multichain (chain_id 0) field, `false` = local.
    pub is_multichain: bool,
    /// The new sequence value to write (old + 1).
    pub new_value: u64,
}

impl Eip8130SequenceUpdate {
    /// Applies this update to the current packed slot value.
    pub fn apply(&self, current: U256) -> U256 {
        let shift = if self.is_multichain { 0 } else { 64 };
        let field_mask: U256 = U256::from(u64::MAX) << shift;
        if self.is_multichain {
            (current & !field_mask) | U256::from(self.new_value)
        } else {
            (current & !field_mask) | (U256::from(self.new_value) << shift)
        }
    }
}

/// Aggregated AA execution data populated during transaction conversion.
///
/// Built from a [`TxEip8130`] in `evm_compat` and consumed by the handler during
/// execution. Non-AA transactions carry a default (empty) instance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Eip8130Parts {
    /// Transaction expiry timestamp (`0` means no expiry).
    pub expiry: u64,
    /// The effective sender address.
    pub sender: Address,
    /// The effective payer address (same as sender if self-pay).
    pub payer: Address,
    /// Authenticated owner identifier from sender auth.
    pub owner_id: B256,
    /// Authenticated owner identifier from payer auth (zero if self-pay).
    pub payer_owner_id: B256,
    /// Nonce key for 2D nonce slot calculation.
    pub nonce_key: U256,
    /// For nonce-free transactions (`nonce_key == NONCE_KEY_MAX`), the hash
    /// used for on-chain replay protection via the expiring-nonce circular
    /// buffer. Computed from `encode_for_sender_signing` — invariant to
    /// signature values. `None` for standard (sequenced) transactions.
    pub nonce_free_hash: Option<B256>,
    /// Whether the tx includes a create entry (determines auto-delegation skip).
    pub has_create_entry: bool,
    /// Explicit delegation target from an `AccountChangeEntry::Delegation`.
    ///
    /// `Some(target)` means the tx requests EIP-7702-style code delegation to
    /// `target`. `Some(Address::ZERO)` clears an existing delegation.
    /// `None` means no delegation entry is present.
    pub delegation_target: Option<Address>,
    /// Total account-change units in the transaction.
    ///
    /// Counting rules:
    /// - each create entry counts as 1,
    /// - each create entry initial owner counts as 1,
    /// - each config operation counts as 1.
    pub account_change_units: usize,
    /// Total owner-change operations across all config-change entries.
    pub total_config_ops: usize,
    /// Gas charged for sender + payer native signature verification.
    pub verification_gas: u64,
    /// Full AA intrinsic gas cost (protocol-computed, non-refundable).
    /// Includes: AA_BASE_COST + calldata + auth SLOADs + verification gas +
    /// nonce key (cold worst-case) + bytecode + config changes.
    ///
    /// The sender's `gas_limit` is execution-only; intrinsic gas is added on
    /// top when computing the revm gas limit. The total revm gas limit is
    /// `aa_intrinsic_gas + tx.gas_limit`.
    pub aa_intrinsic_gas: u64,
    /// Portion of `aa_intrinsic_gas` attributable to payer-specific costs
    /// (`payer_auth_cost` SLOAD + `payer_verification_gas`). The payer's
    /// verifier calls `getMaxCost()` while still running, so `known_intrinsic`
    /// for that precompile is `aa_intrinsic_gas - payer_intrinsic_gas`.
    pub payer_intrinsic_gas: u64,
    /// Maximum gas for custom verifier STATICCALLs. Charged to the payer
    /// separately from both intrinsic gas and the sender's execution
    /// `gas_limit`. Zero when no custom verifiers are used.
    pub custom_verifier_gas_cap: u64,
    /// The sender's verifier address. Used by the handler at inclusion
    /// time to re-validate native verifier ownership via owner_config SLOAD.
    pub sender_verifier: Address,
    /// The payer's verifier address (`Address::ZERO` for self-pay). Used by
    /// the handler for payer ownership re-validation at inclusion time.
    pub payer_verifier: Address,
    /// Auto-delegation code (`0xef0100 || DEFAULT_ACCOUNT_ADDRESS`) if applicable.
    /// Empty if auto-delegation is not needed.
    pub auto_delegation_code: Bytes,
    /// Pre-execution storage writes for account creation (owner registrations).
    /// Applied unconditionally in `validate_against_state`.
    pub pre_writes: Vec<Eip8130StorageWrite>,
    /// Storage writes for config changes (authorize/revoke owners).
    /// Applied in `execution()` only after authorizer chain validation passes.
    pub config_writes: Vec<Eip8130StorageWrite>,
    /// Sequence updates requiring read-modify-write on packed storage slots.
    /// Applied alongside `config_writes` after authorizer validation.
    pub sequence_updates: Vec<Eip8130SequenceUpdate>,
    /// Code placements for account creation (runtime bytecode at CREATE2-derived addresses).
    pub code_placements: Vec<Eip8130CodePlacement>,
    /// Phased call batches. Each inner `Vec` is one atomic phase.
    pub call_phases: Vec<Vec<Eip8130Call>>,
    /// Pre-encoded STATICCALL for custom sender verifier. `None` for native
    /// verifiers (K1, P256, WebAuthn, Delegate) whose verification happens
    /// off-chain. When `Some`, the handler must STATICCALL the verifier
    /// contract before executing call phases.
    pub sender_verify_call: Option<Eip8130VerifyCall>,
    /// Pre-encoded STATICCALL for custom payer verifier. Same semantics.
    pub payer_verify_call: Option<Eip8130VerifyCall>,
    /// Inclusion-time nested-owner revalidation for native delegate sender auth.
    pub sender_delegate_native_validation: Option<Eip8130DelegateNativeValidation>,
    /// Inclusion-time nested-owner revalidation for native delegate payer auth.
    pub payer_delegate_native_validation: Option<Eip8130DelegateNativeValidation>,
    /// Per-config-change authorizer validation data. One entry per
    /// `ConfigChangeEntry` in `account_changes`. The handler validates
    /// these before applying `pre_writes`.
    pub authorizer_validations: Vec<Eip8130AuthorizerValidation>,
    /// System log events for account creation. Emitted during validation
    /// after `pre_writes` and `code_placements` are applied.
    /// Contains `OwnerAuthorized` for each initial owner, then `AccountCreated`.
    pub account_creation_logs: Vec<Eip8130ConfigLog>,
    /// System log events for config changes. Emitted during execution
    /// after `config_writes` and `sequence_updates` are applied.
    /// Contains `OwnerAuthorized`/`OwnerRevoked` per operation.
    pub config_change_logs: Vec<Eip8130ConfigLog>,
    /// `true` when `sender_auth` was empty (e.g. during `eth_estimateGas`).
    /// The handler uses this to add calldata overhead during gas estimation.
    pub sender_auth_empty: bool,
    /// `true` when `payer_auth` was empty on a sponsored transaction.
    pub payer_auth_empty: bool,
}

/// Authorizer validation data for a single config change entry.
///
/// The handler uses this to re-validate that the config change was authorized
/// by an owner with CONFIG scope before applying `config_writes`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Eip8130AuthorizerValidation {
    /// Verifier address from the authorizer's auth prefix.
    pub verifier: Address,
    /// The authenticated owner_id (from native verification at conversion time).
    /// Zero for custom verifiers (determined at runtime via STATICCALL).
    pub owner_id: B256,
    /// STATICCALL data for custom verifiers. `None` for native verifiers.
    pub verify_call: Option<Eip8130VerifyCall>,
    /// The owner changes in this config change (needed for chained validation
    /// where earlier additions become visible to later authorizers).
    pub owner_changes: Vec<Eip8130ConfigOp>,
}

/// Simplified config operation for the handler's in-memory chaining logic.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Eip8130ConfigOp {
    /// `0x01` = authorize, `0x02` = revoke.
    pub change_type: u8,
    /// Verifier contract address.
    pub verifier: Address,
    /// Owner identifier.
    pub owner_id: B256,
    /// Permission scope bitmask.
    pub scope: u8,
}

/// Pre-encoded data for a STATICCALL to `IVerifier.verify(hash, data)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Eip8130VerifyCall {
    /// The verifier contract address.
    pub verifier: Address,
    /// ABI-encoded `IVerifier.verify(hash, data)` calldata.
    pub calldata: Bytes,
    /// The account whose owner_config to check the returned owner_id against.
    pub account: Address,
    /// Required scope bit for the owner (`OwnerScope::SENDER` or `PAYER`).
    pub required_scope: u8,
}

/// Inclusion-time revalidation input for native delegate auth.
///
/// Native delegate auth verifies two owner-config states:
/// - outer binding on the source account (handled by `owner_id` + `sender_verifier` /
///   `payer_verifier`)
/// - nested owner on the delegate account (captured here)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Eip8130DelegateNativeValidation {
    /// Delegate account whose nested owner config must remain valid.
    pub account: Address,
    /// Expected nested verifier (K1/P256/WebAuthn).
    pub verifier: Address,
    /// Nested owner identifier resolved from native signature verification.
    pub owner_id: B256,
}

/// Encodes phase results into the output bytes of an AA transaction.
///
/// Format: one byte per phase, `0x01` = success, `0x00` = failure.
pub fn encode_phase_statuses(results: &[Eip8130PhaseResult]) -> Bytes {
    Bytes::from(results.iter().map(|r| u8::from(r.success)).collect::<Vec<_>>())
}

/// Decodes phase statuses from AA transaction output bytes.
pub fn decode_phase_statuses(output: &[u8]) -> Vec<bool> {
    output.iter().map(|&b| b != 0).collect()
}

/// System log topic for persisting per-phase execution statuses in receipts.
///
/// `keccak256("Eip8130PhaseStatuses(bytes)")`
pub fn phase_statuses_log_topic() -> B256 {
    keccak256(b"Eip8130PhaseStatuses(bytes)")
}

// ── AccountConfiguration contract event topics ───────────────────

/// `keccak256("OwnerAuthorized(address,bytes32,address,uint8)")`
pub(crate) fn owner_authorized_log_topic() -> B256 {
    keccak256(b"OwnerAuthorized(address,bytes32,address,uint8)")
}

/// `keccak256("OwnerRevoked(address,bytes32)")`
pub(crate) fn owner_revoked_log_topic() -> B256 {
    keccak256(b"OwnerRevoked(address,bytes32)")
}

/// `keccak256("AccountCreated(address,bytes32,bytes32)")`
pub(crate) fn account_created_log_topic() -> B256 {
    keccak256(b"AccountCreated(address,bytes32,bytes32)")
}

/// Pads an [`Address`] into a 32-byte indexed topic (left-padded with zeros).
fn address_to_topic(addr: Address) -> B256 {
    let mut topic = B256::ZERO;
    topic.0[12..32].copy_from_slice(addr.as_slice());
    topic
}

/// An `AccountConfiguration` contract event to be injected as a system log.
///
/// The handler applies config changes via direct `sstore` writes, bypassing
/// the Solidity contract. These events are the equivalent Solidity events
/// that wallets and indexers expect to see in receipts.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Eip8130ConfigLog {
    /// `OwnerAuthorized(address indexed account, bytes32 indexed ownerId, address verifier, uint8 scope)`
    OwnerAuthorized {
        /// The account whose owner table is being modified.
        account: Address,
        /// Verifier-derived identifier for the owner.
        owner_id: B256,
        /// Verifier contract that authenticates this owner.
        verifier: Address,
        /// Permission bitmask (0x00 = unrestricted).
        scope: u8,
    },
    /// `OwnerRevoked(address indexed account, bytes32 indexed ownerId)`
    OwnerRevoked {
        /// The account whose owner table is being modified.
        account: Address,
        /// Verifier-derived identifier for the revoked owner.
        owner_id: B256,
    },
    /// `AccountCreated(address indexed account, bytes32 userSalt, bytes32 codeHash)`
    AccountCreated {
        /// The newly deployed account address.
        account: Address,
        /// User-chosen salt for CREATE2 derivation.
        user_salt: B256,
        /// `keccak256(bytecode)` of the deployed runtime code.
        code_hash: B256,
    },
}

/// Converts an [`Eip8130ConfigLog`] into a revm [`Log`] emitted from
/// `emitter` (the AccountConfiguration contract address).
pub fn config_log_to_system_log(emitter: Address, event: &Eip8130ConfigLog) -> Log {
    match event {
        Eip8130ConfigLog::OwnerAuthorized { account, owner_id, verifier, scope } => {
            let mut data = Vec::with_capacity(64);
            let mut verifier_word = [0u8; 32];
            verifier_word[12..32].copy_from_slice(verifier.as_slice());
            data.extend_from_slice(&verifier_word);
            let mut scope_word = [0u8; 32];
            scope_word[31] = *scope;
            data.extend_from_slice(&scope_word);
            Log {
                address: emitter,
                data: LogData::new_unchecked(
                    vec![owner_authorized_log_topic(), address_to_topic(*account), *owner_id],
                    Bytes::from(data),
                ),
            }
        }
        Eip8130ConfigLog::OwnerRevoked { account, owner_id } => Log {
            address: emitter,
            data: LogData::new_unchecked(
                vec![owner_revoked_log_topic(), address_to_topic(*account), *owner_id],
                Bytes::new(),
            ),
        },
        Eip8130ConfigLog::AccountCreated { account, user_salt, code_hash } => {
            let mut data = Vec::with_capacity(64);
            data.extend_from_slice(user_salt.as_slice());
            data.extend_from_slice(code_hash.as_slice());
            Log {
                address: emitter,
                data: LogData::new_unchecked(
                    vec![account_created_log_topic(), address_to_topic(*account)],
                    Bytes::from(data),
                ),
            }
        }
    }
}

/// Creates a system log carrying per-phase execution statuses.
///
/// Emitted from `emitter_address` (typically the TxContext precompile) so that
/// phase statuses survive in the receipt's log list and can be recovered at RPC time.
pub fn phase_statuses_system_log(emitter: Address, results: &[Eip8130PhaseResult]) -> Log {
    let data = Bytes::from(results.iter().map(|r| u8::from(r.success)).collect::<Vec<_>>());
    Log { address: emitter, data: LogData::new_unchecked(vec![phase_statuses_log_topic()], data) }
}

/// Extracts per-phase statuses from a system log emitted during EIP-8130 execution.
///
/// Scans receipt logs for the `Eip8130PhaseStatuses` topic from the expected emitter address.
/// Returns `None` if no matching log is found.
pub fn extract_phase_statuses_from_logs<T: AsRef<Log>>(
    logs: &[T],
    emitter: Address,
) -> Option<Vec<bool>> {
    let topic = phase_statuses_log_topic();
    for log in logs {
        let log = log.as_ref();
        if log.address == emitter && log.topics().first() == Some(&topic) {
            return Some(decode_phase_statuses(&log.data.data));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_all_success() {
        let results = vec![
            Eip8130PhaseResult { success: true, gas_used: 100 },
            Eip8130PhaseResult { success: true, gas_used: 200 },
        ];
        let encoded = encode_phase_statuses(&results);
        assert_eq!(&encoded[..], &[0x01, 0x01]);
    }

    #[test]
    fn encode_all_failure() {
        let results = vec![
            Eip8130PhaseResult { success: false, gas_used: 50 },
            Eip8130PhaseResult { success: false, gas_used: 75 },
        ];
        let encoded = encode_phase_statuses(&results);
        assert_eq!(&encoded[..], &[0x00, 0x00]);
    }

    #[test]
    fn encode_mixed() {
        let results = vec![
            Eip8130PhaseResult { success: true, gas_used: 10 },
            Eip8130PhaseResult { success: false, gas_used: 20 },
            Eip8130PhaseResult { success: true, gas_used: 30 },
        ];
        let encoded = encode_phase_statuses(&results);
        assert_eq!(&encoded[..], &[0x01, 0x00, 0x01]);
    }

    #[test]
    fn encode_empty() {
        let encoded = encode_phase_statuses(&[]);
        assert!(encoded.is_empty());
    }

    #[test]
    fn decode_roundtrip() {
        let results = vec![
            Eip8130PhaseResult { success: true, gas_used: 0 },
            Eip8130PhaseResult { success: false, gas_used: 0 },
            Eip8130PhaseResult { success: true, gas_used: 0 },
            Eip8130PhaseResult { success: false, gas_used: 0 },
        ];
        let encoded = encode_phase_statuses(&results);
        let decoded = decode_phase_statuses(&encoded);
        assert_eq!(decoded, vec![true, false, true, false]);
    }

    #[test]
    fn decode_empty() {
        let decoded = decode_phase_statuses(&[]);
        assert!(decoded.is_empty());
    }

    #[test]
    fn parts_default_is_empty() {
        let parts = Eip8130Parts::default();
        assert_eq!(parts.sender, Address::ZERO);
        assert_eq!(parts.payer, Address::ZERO);
        assert_eq!(parts.owner_id, B256::ZERO);
        assert_eq!(parts.payer_owner_id, B256::ZERO);
        assert_eq!(parts.nonce_key, U256::ZERO);
        assert!(!parts.has_create_entry);
        assert_eq!(parts.account_change_units, 0);
        assert_eq!(parts.verification_gas, 0);
        assert!(parts.auto_delegation_code.is_empty());
        assert!(parts.pre_writes.is_empty());
        assert!(parts.call_phases.is_empty());
        assert!(parts.account_creation_logs.is_empty());
        assert!(parts.config_change_logs.is_empty());
    }

    #[test]
    fn owner_authorized_log_encoding() {
        let emitter = Address::repeat_byte(0xAC);
        let account = Address::repeat_byte(0x01);
        let owner_id = B256::repeat_byte(0x02);
        let verifier = Address::repeat_byte(0x03);
        let scope = 0x0A;

        let event = Eip8130ConfigLog::OwnerAuthorized { account, owner_id, verifier, scope };
        let log = config_log_to_system_log(emitter, &event);

        assert_eq!(log.address, emitter);
        assert_eq!(log.topics().len(), 3);
        assert_eq!(log.topics()[0], owner_authorized_log_topic());
        assert_eq!(log.topics()[1], address_to_topic(account));
        assert_eq!(log.topics()[2], owner_id);

        assert_eq!(log.data.data.len(), 64);
        assert_eq!(&log.data.data[12..32], verifier.as_slice());
        assert_eq!(log.data.data[63], scope);
    }

    #[test]
    fn owner_revoked_log_encoding() {
        let emitter = Address::repeat_byte(0xAC);
        let account = Address::repeat_byte(0x01);
        let owner_id = B256::repeat_byte(0x02);

        let event = Eip8130ConfigLog::OwnerRevoked { account, owner_id };
        let log = config_log_to_system_log(emitter, &event);

        assert_eq!(log.address, emitter);
        assert_eq!(log.topics().len(), 3);
        assert_eq!(log.topics()[0], owner_revoked_log_topic());
        assert_eq!(log.topics()[1], address_to_topic(account));
        assert_eq!(log.topics()[2], owner_id);
        assert!(log.data.data.is_empty());
    }

    #[test]
    fn account_created_log_encoding() {
        let emitter = Address::repeat_byte(0xAC);
        let account = Address::repeat_byte(0x01);
        let user_salt = B256::repeat_byte(0xAA);
        let code_hash = B256::repeat_byte(0xBB);

        let event = Eip8130ConfigLog::AccountCreated { account, user_salt, code_hash };
        let log = config_log_to_system_log(emitter, &event);

        assert_eq!(log.address, emitter);
        assert_eq!(log.topics().len(), 2);
        assert_eq!(log.topics()[0], account_created_log_topic());
        assert_eq!(log.topics()[1], address_to_topic(account));

        assert_eq!(log.data.data.len(), 64);
        assert_eq!(&log.data.data[..32], user_salt.as_slice());
        assert_eq!(&log.data.data[32..64], code_hash.as_slice());
    }

    #[test]
    fn sequence_update_preserves_lock_bits() {
        let mut bytes = [0u8; 32];
        bytes[24..32].copy_from_slice(&1u64.to_be_bytes());
        bytes[16..24].copy_from_slice(&2u64.to_be_bytes());
        bytes[11..16].copy_from_slice(&[0x01, 0x02, 0x03, 0x04, 0x05]);
        bytes[9..11].copy_from_slice(&0x0607u16.to_be_bytes());
        let current = U256::from_be_bytes(bytes);

        let updated =
            Eip8130SequenceUpdate { slot: U256::ZERO, is_multichain: true, new_value: 99 }
                .apply(current);
        let updated_bytes = updated.to_be_bytes::<32>();

        assert_eq!(u64::from_be_bytes(updated_bytes[24..32].try_into().unwrap()), 99);
        assert_eq!(u64::from_be_bytes(updated_bytes[16..24].try_into().unwrap()), 2);
        assert_eq!(&updated_bytes[11..16], &[0x01, 0x02, 0x03, 0x04, 0x05]);
        assert_eq!(&updated_bytes[9..11], &0x0607u16.to_be_bytes());
    }
}
