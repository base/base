//! Handler related to Base chain
use std::{boxed::Box, collections::HashMap};

use revm::{
    context::{
        LocalContextTr,
        journaled_state::{JournalCheckpoint, account::JournaledAccountTr},
        result::InvalidTransaction,
    },
    context_interface::{
        Block, Cfg, ContextTr, JournalTr, Transaction,
        context::ContextError,
        result::{EVMError, ExecutionResult, FromStringError},
    },
    handler::{
        EthFrame, EvmTr, FrameResult, Handler, MainnetHandler,
        evm::FrameTr,
        handler::EvmTrError,
        post_execution::{self, reimburse_caller},
        pre_execution::{calculate_caller_fee, validate_account_nonce_and_code_with_components},
    },
    inspector::{Inspector, InspectorEvmTr, InspectorHandler},
    interpreter::{
        CallOutcome, Gas, InitialAndFloorGas, InstructionResult, InterpreterResult, SharedMemory,
        interpreter::EthInterpreter,
        interpreter_action::{CallInput, CallInputs, CallScheme, CallValue, FrameInit, FrameInput},
    },
    primitives::{Address, B256, Bytes, U256, hardfork::SpecId, keccak256},
};

use crate::{
    Eip8130Parts, Eip8130PhaseResult, Eip8130TxContext, L1BlockInfo, NONCE_MANAGER_ADDRESS,
    OpContextTr, OpHaltReason, OpSpecId, PendingOwnerState, PendingOwnerValidationError,
    TX_CONTEXT_ADDRESS, clear_eip8130_tx_context, config_log_to_system_log,
    constants::{BASE_FEE_RECIPIENT, L1_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT},
    encode_phase_statuses, pending_owner_state_for_change, phase_statuses_system_log,
    set_eip8130_tx_context,
    transaction::{DEPOSIT_TRANSACTION_TYPE, OpTransactionError, OpTxTr},
    validate_pending_owner_state,
};

/// EIP-8130 AA transaction type byte.
const EIP8130_TX_TYPE: u8 = 0x7B;

/// Estimated calldata gas for a K1 auth blob missing during gas estimation.
///
/// K1 auth = 66 bytes (type + 64-byte signature + recovery byte).
/// In RLP, that adds ~67 bytes vs the 1-byte encoding of an empty bytes field.
/// 67 bytes × 16 gas/byte (non-zero) ≈ 1,072, rounded up for safety.
const ESTIMATION_AUTH_CALLDATA_GAS: u64 = 1_100;

/// Gas delta between cold and warm nonce key SSTORE costs.
///
/// `aa_intrinsic_gas` always uses the cold worst-case (22,100). When the nonce
/// channel has been used before the SSTORE cost is only 5,000, so the handler
/// gives back this delta to the call phases at execution time.
const NONCE_COLD_WARM_DELTA: u64 = 17_100;

/// AccountConfiguration deployed contract address.
/// Must match the CREATE2 address from `Deploy.s.sol` (salt = 0).
const ACCOUNT_CONFIG_ADDRESS: Address = Address::new([
    0x4F, 0x20, 0x61, 0x8C, 0xf5, 0xc1, 0x60, 0xe7, 0xAA, 0x38, 0x52, 0x68, 0x72, 0x1d, 0xA9, 0x68,
    0xF8, 0x6F, 0x0e, 0x61,
]);

/// Explicit native K1/ecrecover verifier sentinel (`address(1)`).
///
/// Mirrors `base_alloy_consensus::K1_VERIFIER_ADDRESS` to avoid a cyclic
/// dependency.
const K1_VERIFIER_ADDRESS: Address = Address::new([
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01,
]);

/// Sentinel verifier written when the implicit EOA owner is explicitly revoked.
///
/// Mirrors `base_alloy_consensus::REVOKED_VERIFIER`.
const REVOKED_VERIFIER: Address = Address::new([0xff; 20]);

/// Monotonic cache: once the AccountConfiguration contract is detected, we
/// skip the code-existence check on all subsequent calls. Relaxed ordering
/// is safe because the flag only transitions `false → true`.
static ACCOUNT_CONFIG_DEPLOYED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Base storage slot for the nonce mapping in NonceManager (slot index 1).
const NONCE_BASE_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);
/// Base storage slot for the packed `_accountState` mapping in AccountConfig (slot index 1).
const LOCK_BASE_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);

/// Sentinel nonce key that activates nonce-free mode.
///
/// Mirrors [`base_alloy_consensus::NONCE_KEY_MAX`] to avoid a cyclic dependency.
const NONCE_KEY_MAX: U256 = U256::MAX;

/// Mirrors [`base_alloy_consensus::EXPIRING_SEEN_BASE_SLOT`].
const EXPIRING_SEEN_BASE_SLOT: U256 = U256::from_limbs([2, 0, 0, 0]);
/// Mirrors [`base_alloy_consensus::EXPIRING_RING_BASE_SLOT`].
const EXPIRING_RING_BASE_SLOT: U256 = U256::from_limbs([3, 0, 0, 0]);
/// Mirrors [`base_alloy_consensus::EXPIRING_RING_PTR_SLOT`].
const EXPIRING_RING_PTR_SLOT: U256 = U256::from_limbs([4, 0, 0, 0]);
/// Mirrors [`base_alloy_consensus::EXPIRING_NONCE_SET_CAPACITY`].
const EXPIRING_NONCE_SET_CAPACITY: u32 = 300_000;
/// Mirrors [`base_alloy_consensus::NONCE_FREE_MAX_EXPIRY_WINDOW`].
const NONCE_FREE_MAX_EXPIRY_WINDOW: u64 = 30;

/// Computes the NonceManager storage slot for `nonce[account][nonce_key]`.
///
/// `keccak256(nonce_key . keccak256(account . NONCE_BASE_SLOT))`
///
/// Mirrors [`base_alloy_consensus::nonce_slot`] to avoid a cyclic dependency.
fn aa_nonce_slot(account: Address, nonce_key: U256) -> U256 {
    let inner = {
        let mut buf = [0u8; 64];
        buf[12..32].copy_from_slice(account.as_slice());
        let base_bytes = NONCE_BASE_SLOT.to_be_bytes::<32>();
        buf[32..64].copy_from_slice(&base_bytes);
        keccak256(buf)
    };
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(&nonce_key.to_be_bytes::<32>());
    buf[32..64].copy_from_slice(inner.as_slice());
    U256::from_be_bytes(keccak256(buf).0)
}

/// Computes the storage slot for `expiringNonceSeen[txHash]`.
///
/// Mirrors [`base_alloy_consensus::expiring_seen_slot`] to avoid a cyclic dependency.
fn aa_expiring_seen_slot(tx_hash: B256) -> U256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(tx_hash.as_slice());
    let base = EXPIRING_SEEN_BASE_SLOT.to_be_bytes::<32>();
    buf[32..64].copy_from_slice(&base);
    U256::from_be_bytes(keccak256(buf).0)
}

/// Computes the storage slot for `expiringNonceRing[index]`.
///
/// Mirrors [`base_alloy_consensus::expiring_ring_slot`] to avoid a cyclic dependency.
fn aa_expiring_ring_slot(index: u32) -> U256 {
    let mut buf = [0u8; 64];
    buf[28..32].copy_from_slice(&index.to_be_bytes());
    let base = EXPIRING_RING_BASE_SLOT.to_be_bytes::<32>();
    buf[32..64].copy_from_slice(&base);
    U256::from_be_bytes(keccak256(buf).0)
}

/// Computes the AccountConfig storage slot for `lock_state(account)`.
///
/// Mirrors [`base_alloy_consensus::lock_slot`] to avoid a cyclic dependency.
fn aa_lock_slot(account: Address) -> U256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(account.as_slice());
    let base_bytes = LOCK_BASE_SLOT.to_be_bytes::<32>();
    buf[32..64].copy_from_slice(&base_bytes);
    U256::from_be_bytes(keccak256(buf).0)
}

/// Owner config base storage slot in AccountConfig (slot index 0).
///
/// Solidity declaration:
/// `mapping(bytes32 ownerId => mapping(address account => OwnerConfig)) _ownerConfig`
///
/// `keccak256(account . keccak256(owner_id . 0))`
const OWNER_CONFIG_BASE_SLOT: U256 = U256::ZERO;

/// Computes the AccountConfig storage slot for `owner_config(account, owner_id)`.
///
/// Mirrors [`base_alloy_consensus::owner_config_slot`] to avoid a cyclic dependency.
fn aa_owner_config_slot(account: Address, owner_id: U256) -> U256 {
    let inner = {
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(&owner_id.to_be_bytes::<32>());
        let base_bytes = OWNER_CONFIG_BASE_SLOT.to_be_bytes::<32>();
        buf[32..64].copy_from_slice(&base_bytes);
        keccak256(buf)
    };
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(account.as_slice());
    buf[32..64].copy_from_slice(inner.as_slice());
    U256::from_be_bytes(keccak256(buf).0)
}

/// Parses a packed owner_config word into `(verifier_address, scope)`.
///
/// Layout: `[zeros(11) | scope(1) | verifier(20)]` (big-endian 32 bytes).
fn parse_owner_config_word(word: U256) -> (Address, u8) {
    let bytes = word.to_be_bytes::<32>();
    let scope = bytes[11];
    let verifier = Address::from_slice(&bytes[12..32]);
    (verifier, scope)
}

/// Validates owner authorization against effective state:
/// pending in-tx owner changes first, then on-chain storage.
fn validate_owner_against_effective_config<EVM, ERROR>(
    evm: &mut EVM,
    account: Address,
    owner_id: U256,
    expected_verifier: Address,
    required_scope: u8,
    allow_implicit_eoa: bool,
    pending_owner_overrides: Option<&HashMap<U256, PendingOwnerState>>,
) -> Result<(), ERROR>
where
    EVM: EvmTr<Context: OpContextTr>,
    ERROR: EvmTrError<EVM> + From<OpTransactionError>,
{
    if let Some(state) = pending_owner_overrides.and_then(|m| m.get(&owner_id)) {
        validate_pending_owner_state(state, expected_verifier, required_scope).map_err(|err| {
            let msg: std::borrow::Cow<'static, str> = match err {
                PendingOwnerValidationError::Revoked => {
                    "owner explicitly revoked in pending config changes".into()
                }
                PendingOwnerValidationError::VerifierMismatch { expected, actual } => {
                    format!("verifier mismatch: expected {expected}, got {actual}").into()
                }
                PendingOwnerValidationError::MissingScope { required_scope } => {
                    format!("owner lacks required scope bit 0x{required_scope:02x}").into()
                }
            };
            eip8130_invalid_tx::<ERROR>(msg)
        })?;
        return Ok(());
    }

    evm.ctx().journal_mut().load_account(ACCOUNT_CONFIG_ADDRESS)?;
    let slot = aa_owner_config_slot(account, owner_id);
    let config_word = evm.ctx().journal_mut().sload(ACCOUNT_CONFIG_ADDRESS, slot)?.data;
    let (on_chain_verifier, scope) = parse_owner_config_word(config_word);

    if on_chain_verifier == REVOKED_VERIFIER {
        return Err(eip8130_invalid_tx::<ERROR>(
            "native verifier owner explicitly revoked (REVOKED_VERIFIER sentinel)",
        ));
    }

    if on_chain_verifier == Address::ZERO {
        if allow_implicit_eoa {
            let implicit_owner_id = {
                let mut buf = [0u8; 32];
                buf[..20].copy_from_slice(account.as_slice());
                U256::from_be_bytes(buf)
            };
            if owner_id == implicit_owner_id && expected_verifier == K1_VERIFIER_ADDRESS {
                return Ok(());
            }
        }
        return Err(eip8130_invalid_tx::<ERROR>(
            "owner_config not found and implicit EOA rule does not apply",
        ));
    }

    if on_chain_verifier != expected_verifier {
        return Err(eip8130_invalid_tx::<ERROR>(format!(
            "verifier mismatch: expected {expected_verifier}, got {on_chain_verifier}",
        )));
    }
    if scope != 0 && (scope & required_scope) == 0 {
        return Err(eip8130_invalid_tx::<ERROR>(format!(
            "owner lacks required scope bit 0x{required_scope:02x}",
        )));
    }
    Ok(())
}

/// Reads one sequence value from packed `AccountState { multichain, local, unlocksAt, unlockDelay }`.
fn read_packed_sequence(slot_value: U256, is_multichain: bool) -> u64 {
    if is_multichain { slot_value.as_limbs()[0] } else { (slot_value >> 64_u8).as_limbs()[0] }
}

/// Extra gas to reserve during `eth_estimateGas` for auth blob calldata that
/// will be present in the real transaction but is absent in the estimation
/// request (which uses empty `senderAuth` / `payerAuth`).
fn estimation_calldata_overhead(parts: &Eip8130Parts) -> u64 {
    let mut overhead = 0;
    if parts.sender_auth_empty {
        overhead += ESTIMATION_AUTH_CALLDATA_GAS;
    }
    if parts.payer_auth_empty {
        overhead += ESTIMATION_AUTH_CALLDATA_GAS;
    }
    overhead
}

/// Creates an `InvalidTransaction` error from a message string.
///
/// Produces `EVMError::Transaction(OpTransactionError::Base(InvalidTransaction::Str(...)))`,
/// which the block builder catches and skips (rather than aborting the flashblock).
fn eip8130_invalid_tx<ERROR: From<OpTransactionError>>(
    msg: impl Into<std::borrow::Cow<'static, str>>,
) -> ERROR {
    OpTransactionError::Base(InvalidTransaction::Str(msg.into())).into()
}

/// Validates that `owner_id` is registered in AccountConfig with the expected
/// verifier address and required scope. Returns `Err` on mismatch.
fn validate_owner_config<EVM, ERROR>(
    evm: &mut EVM,
    account: Address,
    owner_id: U256,
    expected_verifier: Address,
    required_scope: u8,
    pending_owner_overrides: Option<&HashMap<U256, PendingOwnerState>>,
) -> Result<(), ERROR>
where
    EVM: EvmTr<Context: OpContextTr>,
    ERROR: EvmTrError<EVM> + From<OpTransactionError>,
{
    validate_owner_against_effective_config::<EVM, ERROR>(
        evm,
        account,
        owner_id,
        expected_verifier,
        required_scope,
        false,
        pending_owner_overrides,
    )
}

/// Re-validates a native verifier's owner_config at inclusion time.
///
/// For `DELEGATE` verifiers this requires two SLOADs: one to resolve the
/// delegation target and another to check the inner verifier's config.
fn validate_native_verifier_owner<EVM, ERROR>(
    evm: &mut EVM,
    account: Address,
    verifier: Address,
    owner_id: B256,
    required_scope: u8,
    pending_owner_overrides: Option<&HashMap<U256, PendingOwnerState>>,
) -> Result<(), ERROR>
where
    EVM: EvmTr<Context: OpContextTr>,
    ERROR: EvmTrError<EVM> + From<OpTransactionError>,
{
    let owner_id_uint = U256::from_be_bytes(owner_id.0);
    let has_pending_override =
        pending_owner_overrides.and_then(|m| m.get(&owner_id_uint)).is_some();

    validate_owner_against_effective_config::<EVM, ERROR>(
        evm,
        account,
        owner_id_uint,
        verifier,
        required_scope,
        true,
        pending_owner_overrides,
    )?;

    if verifier == crate::constants::DELEGATE_VERIFIER_ADDRESS && !has_pending_override {
        // DELEGATE: the on-chain verifier is the delegation target. Read
        // the inner owner's config for the SAME owner_id under the
        // delegation target's verifier address.
        let inner_slot = aa_owner_config_slot(account, owner_id_uint);
        let inner_word = evm.ctx().journal_mut().sload(ACCOUNT_CONFIG_ADDRESS, inner_slot)?.data;
        let (inner_verifier, inner_scope) = parse_owner_config_word(inner_word);

        if inner_verifier == Address::ZERO {
            return Err(eip8130_invalid_tx::<ERROR>("delegate inner verifier owner revoked"));
        }
        if inner_scope != 0 && (inner_scope & required_scope) == 0 {
            return Err(eip8130_invalid_tx::<ERROR>(format!(
                "delegate inner owner lacks required scope 0x{required_scope:02x}"
            )));
        }
    }

    Ok(())
}

/// Re-validates nested owner-config state for native delegate auth.
fn validate_delegate_native_owner<EVM, ERROR>(
    evm: &mut EVM,
    sender: Address,
    delegate: &crate::Eip8130DelegateNativeValidation,
    required_scope: u8,
    pending_sender_owner_overrides: &HashMap<U256, PendingOwnerState>,
) -> Result<(), ERROR>
where
    EVM: EvmTr<Context: OpContextTr>,
    ERROR: EvmTrError<EVM> + From<OpTransactionError>,
{
    let pending_overrides =
        if delegate.account == sender { Some(pending_sender_owner_overrides) } else { None };
    validate_owner_config::<EVM, ERROR>(
        evm,
        delegate.account,
        U256::from_be_bytes(delegate.owner_id.0),
        delegate.verifier,
        required_scope,
        pending_overrides,
    )
}

/// Re-validates config-change preconditions at inclusion time.
///
/// This ensures config updates are still valid even when state changed after
/// mempool admission:
/// - account is not locked
/// - each config-change sequence matches expected on-chain value, with
///   in-tx chaining across multiple entries.
fn validate_config_change_preconditions<EVM, ERROR>(
    evm: &mut EVM,
    sender: Address,
    eip8130: &Eip8130Parts,
) -> Result<(), ERROR>
where
    EVM: EvmTr<Context: OpContextTr>,
    ERROR: EvmTrError<EVM> + From<OpTransactionError>,
{
    if eip8130.sequence_updates.is_empty() && eip8130.config_writes.is_empty() {
        return Ok(());
    }

    if !ACCOUNT_CONFIG_DEPLOYED.load(std::sync::atomic::Ordering::Relaxed) {
        let acct = evm.ctx().journal_mut().load_account_with_code_mut(ACCOUNT_CONFIG_ADDRESS)?.data;
        let has_code = acct.account().info.code_hash != keccak256([]);
        drop(acct);
        if has_code {
            ACCOUNT_CONFIG_DEPLOYED.store(true, std::sync::atomic::Ordering::Relaxed);
        } else {
            return Err(eip8130_invalid_tx::<ERROR>(
                "config changes require AccountConfiguration to be deployed",
            ));
        }
    }

    evm.ctx().journal_mut().load_account(ACCOUNT_CONFIG_ADDRESS)?;

    // Lock-state check: locked accounts cannot process config changes.
    // AccountState packs `unlocksAt` (uint40) at bytes [11..16] of the slot.
    // An account is locked when `block.timestamp < unlocksAt`.
    let lock_slot = aa_lock_slot(sender);
    let lock_word = evm.ctx().journal_mut().sload(ACCOUNT_CONFIG_ADDRESS, lock_slot)?.data;
    let lock_bytes = lock_word.to_be_bytes::<32>();
    let mut ua = [0u8; 8];
    ua[3..8].copy_from_slice(&lock_bytes[11..16]);
    let unlocks_at = u64::from_be_bytes(ua);
    let now: u64 = evm.ctx().block().timestamp().to();
    if now < unlocks_at {
        return Err(eip8130_invalid_tx::<ERROR>("config changes not allowed: account is locked"));
    }

    if eip8130.sequence_updates.is_empty() {
        return Ok(());
    }

    // Sequence check with in-tx chaining.
    let seq_slot = eip8130.sequence_updates[0].slot;
    let packed = evm.ctx().journal_mut().sload(ACCOUNT_CONFIG_ADDRESS, seq_slot)?.data;
    let mut expected_multichain = read_packed_sequence(packed, true);
    let mut expected_local = read_packed_sequence(packed, false);

    for upd in &eip8130.sequence_updates {
        let tx_sequence = upd.new_value.checked_sub(1).ok_or_else(|| {
            eip8130_invalid_tx::<ERROR>("invalid config change sequence (underflow)")
        })?;

        if upd.is_multichain {
            if tx_sequence != expected_multichain {
                return Err(eip8130_invalid_tx::<ERROR>(format!(
                    "config change sequence mismatch: expected {expected_multichain}, got {tx_sequence}"
                )));
            }
            expected_multichain = upd.new_value;
        } else {
            if tx_sequence != expected_local {
                return Err(eip8130_invalid_tx::<ERROR>(format!(
                    "config change sequence mismatch: expected {expected_local}, got {tx_sequence}"
                )));
            }
            expected_local = upd.new_value;
        }
    }

    Ok(())
}

/// Runs a custom verifier STATICCALL and decodes the returned owner_id.
///
/// Charges gas against the transaction's custom-verifier budget via
/// `verification_gas_used`.
fn run_custom_verifier_staticcall<EVM, ERROR, FRAME>(
    mainnet: &mut MainnetHandler<EVM, ERROR, FRAME>,
    evm: &mut EVM,
    verifier: Address,
    calldata: &Bytes,
    caller: Address,
    verification_gas_cap: u64,
    verification_gas_used: &mut u64,
    call_failed_msg: &'static str,
    invalid_owner_id_msg: &'static str,
) -> Result<U256, ERROR>
where
    EVM: EvmTr<Context: OpContextTr, Frame = FRAME>,
    ERROR: EvmTrError<EVM> + From<OpTransactionError>,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    evm.ctx().journal_mut().load_account(verifier)?;

    let call_gas = verification_gas_cap.saturating_sub(*verification_gas_used);
    let call_inputs = CallInputs {
        input: CallInput::Bytes(calldata.clone()),
        return_memory_offset: 0..0,
        gas_limit: call_gas,
        bytecode_address: verifier,
        known_bytecode: None,
        target_address: verifier,
        caller,
        value: CallValue::Transfer(U256::ZERO),
        scheme: CallScheme::StaticCall,
        is_static: true,
    };

    let frame_init = FrameInit {
        depth: 0,
        memory: {
            let ctx = evm.ctx();
            let mut mem = SharedMemory::new_with_buffer(ctx.local().shared_memory_buffer().clone());
            mem.set_memory_limit(ctx.cfg().memory_limit());
            mem
        },
        frame_input: FrameInput::Call(Box::new(call_inputs)),
    };

    let result = mainnet.run_exec_loop(evm, frame_init)?;
    let used = call_gas.saturating_sub(result.gas().remaining());
    *verification_gas_used = verification_gas_used.saturating_add(used);

    if !result.interpreter_result().result.is_ok() {
        return Err(eip8130_invalid_tx::<ERROR>(call_failed_msg));
    }

    let output = result.interpreter_result().output.as_ref();
    if output.len() < 32 {
        return Err(eip8130_invalid_tx::<ERROR>(invalid_owner_id_msg));
    }

    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&output[..32]);
    Ok(U256::from_be_bytes(bytes))
}

/// Validates the config change authorizer chain.
///
/// Iterates `authorizer_validations` in order. For each entry:
/// - Custom verifiers (0x00): STATICCALL the verifier to get `owner_id`, then
///   check `owner_config(sender, owner_id)` for CONFIG scope.
/// - Native verifiers: check the pre-authenticated `owner_id` against
///   `owner_config(sender, owner_id)` for CONFIG scope.
///
/// **Chaining:** AUTHORIZE operations from earlier entries are tracked in an
/// in-memory map. When a later entry's authorizer is a newly-authorized owner,
/// the scope check uses the pending entry rather than an SLOAD. This allows a
/// single tx to chain: entry 1 authorized by existing owner adds new owner X,
/// entry 2 authorized by X does further changes.
///
/// Uses the transaction's `custom_verifier_gas_cap` budget for custom
/// authorizer STATICCALLs. `verification_gas_used` is updated to reflect gas
/// consumed.
fn validate_authorizer_chain<EVM, ERROR, FRAME>(
    mainnet: &mut MainnetHandler<EVM, ERROR, FRAME>,
    evm: &mut EVM,
    sender: Address,
    eip8130: &Eip8130Parts,
    verification_gas_used: &mut u64,
) -> Result<HashMap<U256, PendingOwnerState>, ERROR>
where
    EVM: EvmTr<Context: OpContextTr, Frame = FRAME>,
    ERROR: EvmTrError<EVM> + From<OpTransactionError>,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    if eip8130.authorizer_validations.is_empty() {
        return Ok(HashMap::new());
    }

    // Pending additions from earlier entries in the chain.
    // Maps owner_id -> effective pending owner state.
    let mut pending_owners: HashMap<U256, PendingOwnerState> = HashMap::new();

    for validation in &eip8130.authorizer_validations {
        let owner_id = if let Some(verify_call) = &validation.verify_call {
            // Custom verifier: STATICCALL to get owner_id.
            run_custom_verifier_staticcall::<EVM, ERROR, FRAME>(
                mainnet,
                evm,
                verify_call.verifier,
                &verify_call.calldata,
                sender,
                eip8130.custom_verifier_gas_cap,
                verification_gas_used,
                "config change authorizer STATICCALL failed",
                "config change authorizer returned invalid owner_id",
            )?
        } else {
            // Native verifier: owner_id was pre-authenticated at conversion time.
            U256::from_be_bytes(validation.owner_id.0)
        };

        if owner_id.is_zero() {
            return Err(eip8130_invalid_tx::<ERROR>(
                "config change authorizer returned zero owner_id",
            ));
        }

        // Check CONFIG scope with pending overrides first, then on-chain.
        validate_owner_against_effective_config::<EVM, ERROR>(
            evm,
            sender,
            owner_id,
            validation.verifier,
            crate::constants::OWNER_SCOPE_CONFIG,
            true,
            Some(&pending_owners),
        )?;

        // Record pending additions from this entry for chaining.
        for op in &validation.owner_changes {
            if let Some(state) =
                pending_owner_state_for_change(op.change_type, op.verifier, op.scope)
            {
                pending_owners.insert(U256::from_be_bytes(op.owner_id.0), state);
            }
        }
    }

    Ok(pending_owners)
}

/// Base handler extends the [`Handler`] with Base-specific logic.
#[derive(Debug, Clone)]
pub struct OpHandler<EVM, ERROR, FRAME> {
    /// Mainnet handler allows us to use functions from the mainnet handler inside the Base handler.
    /// So we dont duplicate the logic
    pub mainnet: MainnetHandler<EVM, ERROR, FRAME>,
}

impl<EVM, ERROR, FRAME> OpHandler<EVM, ERROR, FRAME> {
    /// Create a new Base handler.
    pub fn new() -> Self {
        Self { mainnet: MainnetHandler::default() }
    }
}

impl<EVM, ERROR, FRAME> Default for OpHandler<EVM, ERROR, FRAME> {
    fn default() -> Self {
        Self::new()
    }
}

/// Trait to check if the error is a transaction error.
///
/// Used in `cache_error` handler to catch deposit transaction that was halted.
pub trait IsTxError {
    /// Check if the error is a transaction error.
    fn is_tx_error(&self) -> bool;
}

impl<DB, TX> IsTxError for EVMError<DB, TX> {
    fn is_tx_error(&self) -> bool {
        matches!(self, Self::Transaction(_))
    }
}

impl<EVM, ERROR, FRAME> Handler for OpHandler<EVM, ERROR, FRAME>
where
    EVM: EvmTr<Context: OpContextTr, Frame = FRAME>,
    ERROR: EvmTrError<EVM> + From<OpTransactionError> + FromStringError + IsTxError,
    // TODO `FrameResult` should be a generic trait.
    // TODO `FrameInit` should be a generic.
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    type Evm = EVM;
    type Error = ERROR;
    type HaltReason = OpHaltReason;

    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        // Do not perform any extra validation for deposit transactions, they are pre-verified on L1.
        let ctx = evm.ctx();
        let tx = ctx.tx();
        let tx_type = tx.tx_type();
        if tx_type == DEPOSIT_TRANSACTION_TYPE {
            // Do not allow for a system transaction to be processed if Regolith is enabled.
            if tx.is_system_transaction()
                && evm.ctx().cfg().spec().is_enabled_in(OpSpecId::REGOLITH)
            {
                return Err(OpTransactionError::DepositSystemTxPostRegolith.into());
            }
            return Ok(());
        }

        // Check that non-deposit transactions have enveloped_tx set
        if tx.enveloped_tx().is_none() {
            return Err(OpTransactionError::MissingEnvelopedTx.into());
        }

        // AA transactions require BASE_V1. Reject if the spec is not active.
        if tx_type == EIP8130_TX_TYPE {
            if !evm.ctx().cfg().spec().is_enabled_in(OpSpecId::BASE_V1) {
                return Err(OpTransactionError::Base(InvalidTransaction::Str(
                    "EIP-8130 AA transactions require BASE_V1".into(),
                ))
                .into());
            }

            // Keep canonical env checks (including chain-id checks) aligned
            // with non-AA transaction validation.
            self.mainnet.validate_env(evm)?;

            let ctx = evm.ctx();

            if !ctx.cfg().is_base_fee_check_disabled() {
                let basefee = ctx.block().basefee() as u128;
                let max_fee = ctx.tx().max_fee_per_gas();
                let max_priority = ctx.tx().max_priority_fee_per_gas().unwrap_or(0);

                if max_fee < basefee {
                    return Err(OpTransactionError::Base(InvalidTransaction::Str(
                        "EIP-8130: max_fee_per_gas below base fee".into(),
                    ))
                    .into());
                }
                if max_priority > max_fee {
                    return Err(OpTransactionError::Base(InvalidTransaction::Str(
                        "EIP-8130: max_priority_fee_per_gas exceeds max_fee_per_gas".into(),
                    ))
                    .into());
                }
            }

            // Inclusion-time expiry check (defense-in-depth against bypassing
            // mempool admission).
            let expiry = ctx.tx().eip8130_parts().expiry;
            if expiry != 0 {
                let block_ts = ctx.block().timestamp().saturating_to::<u64>();
                if block_ts > expiry {
                    return Err(OpTransactionError::Base(InvalidTransaction::Str(
                        format!(
                            "EIP-8130: transaction expired (expiry={expiry}, current={block_ts})"
                        )
                        .into(),
                    ))
                    .into());
                }
            }

            // Inclusion-time structural guard for phased calls.
            let total_calls: usize =
                ctx.tx().eip8130_parts().call_phases.iter().map(Vec::len).sum();
            if total_calls > crate::constants::MAX_CALLS_PER_TX {
                return Err(OpTransactionError::Base(InvalidTransaction::Str(
                    format!(
                        "EIP-8130: too many calls ({total_calls} > {})",
                        crate::constants::MAX_CALLS_PER_TX
                    )
                    .into(),
                ))
                .into());
            }

            let total_account_changes = ctx.tx().eip8130_parts().account_change_units;
            if total_account_changes > crate::constants::MAX_ACCOUNT_CHANGES_PER_TX {
                return Err(OpTransactionError::Base(InvalidTransaction::Str(
                    format!(
                        "EIP-8130: too many account changes ({total_account_changes} > {})",
                        crate::constants::MAX_ACCOUNT_CHANGES_PER_TX
                    )
                    .into(),
                ))
                .into());
            }
            let total_config_ops = ctx.tx().eip8130_parts().total_config_ops;
            if total_config_ops > crate::constants::MAX_CONFIG_OPS_PER_TX {
                return Err(OpTransactionError::Base(InvalidTransaction::Str(
                    format!(
                        "EIP-8130: too many config ops ({total_config_ops} > {})",
                        crate::constants::MAX_CONFIG_OPS_PER_TX
                    )
                    .into(),
                ))
                .into());
            }

            return Ok(());
        }

        self.mainnet.validate_env(evm)
    }

    fn validate_initial_tx_gas(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<InitialAndFloorGas, Self::Error> {
        if evm.ctx().tx().tx_type() == EIP8130_TX_TYPE {
            let ctx = evm.ctx();
            let parts = ctx.tx().eip8130_parts();
            let aa_gas = parts.aa_intrinsic_gas;
            let calldata_overhead = estimation_calldata_overhead(parts);
            let is_estimation = ctx.cfg().is_base_fee_check_disabled();
            let gas_limit = ctx.tx().gas_limit();

            let effective_gas = if is_estimation { aa_gas + calldata_overhead } else { aa_gas };

            if effective_gas > gas_limit {
                return Err(InvalidTransaction::CallGasCostMoreThanGasLimit {
                    gas_limit,
                    initial_gas: effective_gas,
                }
                .into());
            }
            return Ok(InitialAndFloorGas::new(effective_gas, 0));
        }
        self.mainnet.validate_initial_tx_gas(evm)
    }

    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<(), Self::Error> {
        let (block, tx, cfg, journal, chain, _) = evm.ctx().all_mut();
        let spec = cfg.spec();

        if tx.tx_type() == DEPOSIT_TRANSACTION_TYPE {
            let basefee = block.basefee() as u128;
            let blob_price = block.blob_gasprice().unwrap_or_default();
            // deposit skips max fee check and just deducts the effective balance spending.

            let mut caller = journal.load_account_with_code_mut(tx.caller())?.data;

            let effective_balance_spending = tx
                .effective_balance_spending(basefee, blob_price)
                .expect("Deposit transaction effective balance spending overflow")
                - tx.value();

            // Mind value should be added first before subtracting the effective balance spending.
            let mut new_balance = caller
                .balance()
                .saturating_add(U256::from(tx.mint().unwrap_or_default()))
                .saturating_sub(effective_balance_spending);

            if cfg.is_balance_check_disabled() {
                // Make sure the caller's balance is at least the value of the transaction.
                // this is not consensus critical, and it is used in testing.
                new_balance = new_balance.max(tx.value());
            }

            // set the new balance and bump the nonce if it is a call
            caller.set_balance(new_balance);
            if tx.kind().is_call() {
                caller.bump_nonce();
            }

            return Ok(());
        }

        // L1 block info is stored in the context for later use.
        // and it will be reloaded from the database if it is not for the current block.
        if chain.l2_block != Some(block.number()) {
            *chain = L1BlockInfo::try_fetch(journal.db_mut(), block.number(), spec)?;
        }

        // Clear any stale EIP-8130 context from a previous transaction.
        clear_eip8130_tx_context();

        // AA transactions: deduct gas from payer, increment NonceManager nonce,
        // auto-delegate bare EOAs, and apply pre-execution storage writes.
        if tx.tx_type() == EIP8130_TX_TYPE {
            let sender = tx.caller();
            let nonce_sequence = tx.nonce();
            let eip8130 = tx.eip8130_parts().clone();

            {
                let execution_gas_limit = tx.gas_limit().saturating_sub(eip8130.aa_intrinsic_gas);
                let known_intrinsic =
                    eip8130.aa_intrinsic_gas.saturating_sub(eip8130.payer_intrinsic_gas);
                set_eip8130_tx_context(Eip8130TxContext::new(
                    &eip8130,
                    execution_gas_limit,
                    known_intrinsic,
                    U256::from(tx.max_fee_per_gas()),
                ));
            }

            // --- Gas deduction from payer ---
            let payer = eip8130.payer;
            let mut payer_account = journal.load_account_with_code_mut(payer)?.data;
            let mut balance = payer_account.account().info.balance;

            if !cfg.is_fee_charge_disabled() {
                let Some(additional_cost) = chain.tx_cost_with_tx(tx, spec) else {
                    return Err(ERROR::from_string(
                        "[OPTIMISM] Failed to load enveloped transaction.".into(),
                    ));
                };
                let Some(new_balance) = balance.checked_sub(additional_cost) else {
                    return Err(InvalidTransaction::LackOfFundForMaxFee {
                        fee: Box::new(additional_cost),
                        balance: Box::new(balance),
                    }
                    .into());
                };
                balance = new_balance;
            }

            let balance = calculate_caller_fee(balance, tx, block, cfg)?;
            payer_account.set_balance(balance);
            drop(payer_account);

            // Check if sender is a bare EOA (no code) for auto-delegation.
            let sender_account = journal.load_account_with_code_mut(sender)?.data;
            let sender_has_code = sender_account.account().info.code_hash != keccak256([]);
            drop(sender_account);

            // --- Nonce validation and increment in NonceManager ---
            let nonce_key = eip8130.nonce_key;
            if nonce_key != NONCE_KEY_MAX {
                let slot = aa_nonce_slot(sender, nonce_key);

                journal.load_account(NONCE_MANAGER_ADDRESS)?;
                let current_seq = journal.sload(NONCE_MANAGER_ADDRESS, slot)?.data;

                let skip_nonce_check =
                    cfg.is_nonce_check_disabled() || cfg.is_base_fee_check_disabled();

                if !skip_nonce_check {
                    let expected = U256::from(nonce_sequence);
                    if current_seq != expected {
                        if current_seq > expected {
                            return Err(InvalidTransaction::NonceTooLow {
                                tx: nonce_sequence,
                                state: current_seq.as_limbs()[0],
                            }
                            .into());
                        }
                        return Err(InvalidTransaction::NonceTooHigh {
                            tx: nonce_sequence,
                            state: current_seq.as_limbs()[0],
                        }
                        .into());
                    }
                }
                let next_seq = if skip_nonce_check {
                    current_seq + U256::from(1)
                } else {
                    U256::from(nonce_sequence + 1)
                };
                journal.sstore(NONCE_MANAGER_ADDRESS, slot, next_seq)?;
            } else {
                // --- Expiring-nonce circular buffer (nonce-free mode) ---
                //
                // On-chain replay protection: a fixed-capacity ring buffer
                // stores recent nonce-free tx hashes keyed by expiry. Before
                // including a new nonce-free tx the handler checks that no
                // active (unexpired) entry with the same hash exists, evicts
                // the oldest entry if it has expired, and inserts the new one.
                let now: u64 = block.timestamp().to();
                let expiry = eip8130.expiry;
                let skip_checks = cfg.is_nonce_check_disabled() || cfg.is_base_fee_check_disabled();

                if !skip_checks {
                    if expiry <= now || expiry > now + NONCE_FREE_MAX_EXPIRY_WINDOW {
                        return Err(ERROR::from_string(format!(
                            "nonce-free expiry out of window: expiry={expiry}, now={now}"
                        )));
                    }
                }

                let nf_hash = eip8130.nonce_free_hash.unwrap_or_default();
                journal.load_account(NONCE_MANAGER_ADDRESS)?;

                // Replay check
                let seen_slot = aa_expiring_seen_slot(nf_hash);
                let seen_expiry = journal.sload(NONCE_MANAGER_ADDRESS, seen_slot)?.data;
                if !skip_checks && seen_expiry != U256::ZERO && seen_expiry > U256::from(now) {
                    return Err(ERROR::from_string(
                        "nonce-free transaction replay: hash already seen".into(),
                    ));
                }

                // Read ring buffer pointer
                let ptr_raw = journal.sload(NONCE_MANAGER_ADDRESS, EXPIRING_RING_PTR_SLOT)?.data;
                let idx = ptr_raw.as_limbs()[0] as u32 % EXPIRING_NONCE_SET_CAPACITY;

                // Read existing entry at this ring position
                let ring_slot = aa_expiring_ring_slot(idx);
                let old_hash_raw = journal.sload(NONCE_MANAGER_ADDRESS, ring_slot)?.data;

                // Evict old entry if present (must be expired)
                if old_hash_raw != U256::ZERO {
                    let old_hash = B256::from(old_hash_raw.to_be_bytes::<32>());
                    let old_seen_slot = aa_expiring_seen_slot(old_hash);
                    let old_expiry = journal.sload(NONCE_MANAGER_ADDRESS, old_seen_slot)?.data;
                    if !skip_checks && old_expiry != U256::ZERO && old_expiry > U256::from(now) {
                        return Err(ERROR::from_string(
                            "nonce-free buffer full: cannot evict unexpired entry".into(),
                        ));
                    }
                    journal.sstore(NONCE_MANAGER_ADDRESS, old_seen_slot, U256::ZERO)?;
                }

                // Insert new entry into ring + seen set
                journal.sstore(NONCE_MANAGER_ADDRESS, ring_slot, U256::from_be_bytes(nf_hash.0))?;
                journal.sstore(NONCE_MANAGER_ADDRESS, seen_slot, U256::from(expiry))?;

                // Advance pointer (wraps at capacity)
                let next_ptr = if idx + 1 >= EXPIRING_NONCE_SET_CAPACITY {
                    U256::ZERO
                } else {
                    U256::from(idx + 1)
                };
                journal.sstore(NONCE_MANAGER_ADDRESS, EXPIRING_RING_PTR_SLOT, next_ptr)?;
            }

            // --- Delegation ---
            // Explicit entry (account_changes type 0x02) takes priority.
            // Otherwise bare EOAs get auto-delegated to DEFAULT_ACCOUNT.
            //
            // Uses revm's native EIP-7702 `Bytecode::new_eip7702` so the
            // code is recognized as delegation by `is_eip7702()` throughout
            // the EVM (EXTCODESIZE, EXTCODECOPY, EXTCODEHASH, etc.).
            if let Some(target) = eip8130.delegation_target {
                let acc = journal.load_account_with_code_mut(sender)?.data;
                let current_code = acc.account().info.code.as_ref();
                let is_empty = current_code.map_or(true, |c| c.is_empty());
                let is_delegation = current_code.map_or(false, |c| c.is_eip7702());
                drop(acc);

                if !is_empty && !is_delegation {
                    return Err(ERROR::from_string(
                        "delegation entry rejected: sender has non-delegation bytecode".into(),
                    ));
                }

                let code = if target.is_zero() {
                    revm::bytecode::Bytecode::default()
                } else {
                    revm::bytecode::Bytecode::new_eip7702(target)
                };
                let mut acc = journal.load_account_with_code_mut(sender)?.data;
                acc.set_code_and_hash_slow(code);
                drop(acc);
            } else if !sender_has_code
                && !eip8130.has_create_entry
                && eip8130.auto_delegation_code.len() == 23
            {
                let target = Address::from_slice(&eip8130.auto_delegation_code[3..]);
                let code = revm::bytecode::Bytecode::new_eip7702(target);
                let mut acc = journal.load_account_with_code_mut(sender)?.data;
                acc.set_code_and_hash_slow(code);
                drop(acc);
            }

            // --- Apply pre-execution storage writes (account creation only) ---
            // Owner registrations for newly created accounts.
            // Config change writes are deferred to execution() after authorizer
            // chain validation.
            for w in &eip8130.pre_writes {
                journal.load_account(w.address)?;
                journal.sstore(w.address, w.slot, w.value)?;
            }

            // --- Account creation (place runtime bytecode at CREATE2-derived addresses) ---
            for placement in &eip8130.code_placements {
                let code = revm::bytecode::Bytecode::new_raw(placement.code.clone());
                let mut acc = journal.load_account_with_code_mut(placement.address)?.data;
                acc.set_code_and_hash_slow(code);
                drop(acc);
            }

            // --- Emit AccountConfiguration events for account creation ---
            for event in &eip8130.account_creation_logs {
                journal.log(config_log_to_system_log(ACCOUNT_CONFIG_ADDRESS, event));
            }

            return Ok(());
        }

        let mut caller_account = journal.load_account_with_code_mut(tx.caller())?.data;

        // validates account nonce and code
        validate_account_nonce_and_code_with_components(&caller_account.account().info, tx, cfg)?;

        // check additional cost and deduct it from the caller's balances
        let mut balance = caller_account.account().info.balance;

        if !cfg.is_fee_charge_disabled() {
            let Some(additional_cost) = chain.tx_cost_with_tx(tx, spec) else {
                return Err(ERROR::from_string(
                    "[OPTIMISM] Failed to load enveloped transaction.".into(),
                ));
            };
            let Some(new_balance) = balance.checked_sub(additional_cost) else {
                return Err(InvalidTransaction::LackOfFundForMaxFee {
                    fee: Box::new(additional_cost),
                    balance: Box::new(balance),
                }
                .into());
            };
            balance = new_balance
        }

        let balance = calculate_caller_fee(balance, tx, block, cfg)?;

        // make changes to the account
        caller_account.set_balance(balance);
        if tx.kind().is_call() {
            caller_account.bump_nonce();
        }

        Ok(())
    }

    fn execution(
        &mut self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &InitialAndFloorGas,
    ) -> Result<FrameResult, Self::Error> {
        if evm.ctx().tx().tx_type() != EIP8130_TX_TYPE {
            return self.mainnet.execution(evm, init_and_floor_gas);
        }

        let eip8130 = evm.ctx().tx().eip8130_parts().clone();
        let sender = evm.ctx().tx().caller();

        // In estimation / eth_call mode we skip signature verification and
        // config validation since dummy (empty) auth blobs are expected.
        let is_estimation = evm.ctx().cfg().is_base_fee_check_disabled();

        // Determine whether the nonce channel is warm (previously used).
        // validate_against_state_and_deduct_caller already incremented the
        // nonce, so the current slot value is `original + 1`. If > 1 the
        // original was non-zero → warm SSTORE.
        //
        // Only adjust for real transactions — during estimation the handler
        // must stay consistent with validate_initial_tx_gas (which always
        // uses cold gas) so the binary search doesn't break.
        let nonce_warm_adjustment = if !is_estimation && eip8130.nonce_key != NONCE_KEY_MAX {
            let nonce_slot = aa_nonce_slot(sender, eip8130.nonce_key);
            let nonce_value =
                evm.ctx().journal_mut().sload(NONCE_MANAGER_ADDRESS, nonce_slot)?.data;
            if nonce_value > U256::from(1) { NONCE_COLD_WARM_DELTA } else { 0 }
        } else {
            0
        };

        // Strip intrinsic and custom verifier cap from the revm gas_limit to
        // recover the sender's execution-only budget. During estimation, also
        // reserve calldata gas for auth blobs that will be present in the real tx.
        let overhead = if is_estimation { estimation_calldata_overhead(&eip8130) } else { 0 };
        let gas_limit = evm
            .ctx()
            .tx()
            .gas_limit()
            .saturating_sub(eip8130.aa_intrinsic_gas + eip8130.custom_verifier_gas_cap + overhead)
            .saturating_add(nonce_warm_adjustment);

        let mut gas_remaining = gas_limit;
        let mut phase_results = Vec::with_capacity(eip8130.call_phases.len());

        // Ensure sender is loaded in the journal state for sub-call transfers.
        evm.ctx().journal_mut().load_account(sender)?;

        // Track gas used by custom verifier STATICCALLs. This is charged to
        // the payer separately from the sender's execution budget, capped at
        // `custom_verifier_gas_cap`.
        let mut verification_gas_used: u64 = 0;

        if !is_estimation {
            validate_config_change_preconditions::<EVM, ERROR>(evm, sender, &eip8130)?;

            // Validate config-change authorizer chain first against on-chain state.
            // This yields the pending owner overlay that is considered for
            // subsequent sender/payer auth checks.
            let pending_sender_owner_overrides = validate_authorizer_chain::<EVM, ERROR, FRAME>(
                &mut self.mainnet,
                evm,
                sender,
                &eip8130,
                &mut verification_gas_used,
            )?;

            // --- Custom verifier STATICCALL verification ---
            // For custom verifiers we STATICCALL to resolve owner_id, then verify
            // ownership/scope against effective state (pending owner changes first,
            // then on-chain).
            let verify_calls =
                [eip8130.sender_verify_call.as_ref(), eip8130.payer_verify_call.as_ref()];
            for verify_call in verify_calls.into_iter().flatten() {
                let owner_id = run_custom_verifier_staticcall::<EVM, ERROR, FRAME>(
                    &mut self.mainnet,
                    evm,
                    verify_call.verifier,
                    &verify_call.calldata,
                    sender,
                    eip8130.custom_verifier_gas_cap,
                    &mut verification_gas_used,
                    "custom verifier STATICCALL failed",
                    "custom verifier returned invalid owner_id (< 32 bytes)",
                )?;

                let pending_overrides = if verify_call.account == sender {
                    Some(&pending_sender_owner_overrides)
                } else {
                    None
                };
                validate_owner_config::<EVM, ERROR>(
                    evm,
                    verify_call.account,
                    owner_id,
                    verify_call.verifier,
                    verify_call.required_scope,
                    pending_overrides,
                )?;
            }

            // Delegate with nested custom verifier: after nested owner validation,
            // validate the outer delegate owner binding on sender/payer account.
            if eip8130.sender_verify_call.is_some()
                && eip8130.sender_verifier == crate::constants::DELEGATE_VERIFIER_ADDRESS
                && eip8130.owner_id != B256::ZERO
            {
                validate_owner_config::<EVM, ERROR>(
                    evm,
                    sender,
                    U256::from_be_bytes(eip8130.owner_id.0),
                    crate::constants::DELEGATE_VERIFIER_ADDRESS,
                    crate::constants::OWNER_SCOPE_SENDER,
                    Some(&pending_sender_owner_overrides),
                )?;
            }
            if eip8130.payer_verify_call.is_some()
                && eip8130.payer_verifier == crate::constants::DELEGATE_VERIFIER_ADDRESS
                && eip8130.payer_owner_id != B256::ZERO
                && eip8130.payer != eip8130.sender
            {
                let payer_pending_overrides = if eip8130.payer == sender {
                    Some(&pending_sender_owner_overrides)
                } else {
                    None
                };
                validate_owner_config::<EVM, ERROR>(
                    evm,
                    eip8130.payer,
                    U256::from_be_bytes(eip8130.payer_owner_id.0),
                    crate::constants::DELEGATE_VERIFIER_ADDRESS,
                    crate::constants::OWNER_SCOPE_PAYER,
                    payer_pending_overrides,
                )?;
            }

            // --- Native verifier re-validation at inclusion ---
            // Validate sender/payer auth against effective state after considering
            // pending in-tx owner changes.
            if eip8130.sender_verify_call.is_none() && eip8130.sender_verifier != Address::ZERO {
                validate_native_verifier_owner::<EVM, ERROR>(
                    evm,
                    sender,
                    eip8130.sender_verifier,
                    eip8130.owner_id,
                    crate::constants::OWNER_SCOPE_SENDER,
                    Some(&pending_sender_owner_overrides),
                )?;
            }
            if eip8130.payer_verify_call.is_none()
                && eip8130.payer_verifier != Address::ZERO
                && eip8130.payer != eip8130.sender
            {
                let payer_pending_overrides = if eip8130.payer == sender {
                    Some(&pending_sender_owner_overrides)
                } else {
                    None
                };
                validate_native_verifier_owner::<EVM, ERROR>(
                    evm,
                    eip8130.payer,
                    eip8130.payer_verifier,
                    eip8130.payer_owner_id,
                    crate::constants::OWNER_SCOPE_PAYER,
                    payer_pending_overrides,
                )?;
            }

            // Native delegate auth also depends on nested owner state on the
            // delegate account. Re-check it at inclusion time.
            if let Some(delegate) = &eip8130.sender_delegate_native_validation {
                validate_delegate_native_owner::<EVM, ERROR>(
                    evm,
                    sender,
                    delegate,
                    crate::constants::OWNER_SCOPE_SENDER,
                    &pending_sender_owner_overrides,
                )?;
            }
            if let Some(delegate) = &eip8130.payer_delegate_native_validation {
                validate_delegate_native_owner::<EVM, ERROR>(
                    evm,
                    sender,
                    delegate,
                    crate::constants::OWNER_SCOPE_PAYER,
                    &pending_sender_owner_overrides,
                )?;
            }
        }

        // --- Apply config change writes + sequence bumps ---
        // Only reached after authorizer chain validation passes. If any
        // authorizer fails, the error above causes the tx to be invalid
        // and none of these writes are applied.
        if !eip8130.config_writes.is_empty() {
            for w in &eip8130.config_writes {
                evm.ctx().journal_mut().load_account(w.address)?;
                evm.ctx().journal_mut().sstore(w.address, w.slot, w.value)?;
            }
        }
        if !eip8130.sequence_updates.is_empty() {
            evm.ctx().journal_mut().load_account(ACCOUNT_CONFIG_ADDRESS)?;
            for upd in &eip8130.sequence_updates {
                let current = evm.ctx().journal_mut().sload(ACCOUNT_CONFIG_ADDRESS, upd.slot)?.data;
                let new_packed = upd.apply(current);
                evm.ctx().journal_mut().sstore(ACCOUNT_CONFIG_ADDRESS, upd.slot, new_packed)?;
            }
        }

        // --- Emit AccountConfiguration events for config changes ---
        for event in &eip8130.config_change_logs {
            evm.ctx().journal_mut().log(config_log_to_system_log(ACCOUNT_CONFIG_ADDRESS, event));
        }

        // Refund unused verification gas budget back to Gas.
        let unused_verification_gas =
            eip8130.custom_verifier_gas_cap.saturating_sub(verification_gas_used);

        let mut accumulated_refunds: i64 = 0;

        for phase in &eip8130.call_phases {
            let checkpoint = evm.ctx().journal_mut().checkpoint();
            let mut phase_ok = true;
            let phase_gas_start = gas_remaining;
            let mut phase_refunds: i64 = 0;

            for call in phase {
                if gas_remaining == 0 {
                    phase_ok = false;
                    break;
                }

                evm.ctx().journal_mut().load_account(call.to)?;

                let call_gas = gas_remaining;
                let call_inputs = CallInputs {
                    input: CallInput::Bytes(call.data.clone()),
                    return_memory_offset: 0..0,
                    gas_limit: call_gas,
                    bytecode_address: call.to,
                    known_bytecode: None,
                    target_address: call.to,
                    caller: sender,
                    value: CallValue::Transfer(call.value),
                    scheme: CallScheme::Call,
                    is_static: false,
                };

                let frame_init = FrameInit {
                    depth: 0,
                    memory: {
                        let ctx = evm.ctx();
                        let mut mem = SharedMemory::new_with_buffer(
                            ctx.local().shared_memory_buffer().clone(),
                        );
                        mem.set_memory_limit(ctx.cfg().memory_limit());
                        mem
                    },
                    frame_input: FrameInput::Call(Box::new(call_inputs)),
                };

                let call_result = self.mainnet.run_exec_loop(evm, frame_init)?;
                let call_gas_used = call_gas.saturating_sub(call_result.gas().remaining());
                gas_remaining = gas_remaining.saturating_sub(call_gas_used);
                phase_refunds += call_result.gas().refunded();

                if !call_result.interpreter_result().result.is_ok() {
                    phase_ok = false;
                    break;
                }
            }

            if phase_ok {
                accumulated_refunds += phase_refunds;
            } else {
                evm.ctx().journal_mut().checkpoint_revert(checkpoint);
            }

            phase_results.push(Eip8130PhaseResult {
                success: phase_ok,
                gas_used: phase_gas_start.saturating_sub(gas_remaining),
            });
        }

        let any_phase_succeeded = phase_results.iter().any(|r| r.success);

        // Deploy-only transactions (account creation with no call phases) succeed
        // when pre-execution code placement completed without error.
        let deploy_only_success = phase_results.is_empty() && !eip8130.code_placements.is_empty();

        let tx_succeeded = is_estimation || any_phase_succeeded || deploy_only_success;

        // Emit a system log with per-phase statuses so they survive in the receipt's
        // log list and can be recovered at RPC time. Always emitted when phases
        // exist so `extract_phase_statuses_from_logs` returns authoritative data
        // regardless of the tx outcome.
        if !phase_results.is_empty() {
            evm.ctx()
                .journal_mut()
                .log(phase_statuses_system_log(TX_CONTEXT_ADDRESS, &phase_results));
        }

        let mut result_gas = Gas::new_spent(evm.ctx().tx().gas_limit());
        result_gas.erase_cost(gas_remaining + unused_verification_gas);
        if accumulated_refunds > 0 {
            result_gas.record_refund(accumulated_refunds);
        }

        let output = encode_phase_statuses(&phase_results);

        let instruction_result =
            if tx_succeeded { InstructionResult::Stop } else { InstructionResult::Revert };

        let mut frame_result = FrameResult::Call(CallOutcome::new(
            InterpreterResult { result: instruction_result, output, gas: result_gas },
            0..0,
        ));

        self.last_frame_result(evm, &mut frame_result)?;
        Ok(frame_result)
    }

    fn last_frame_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        let ctx = evm.ctx();
        let tx = ctx.tx();
        let is_deposit = tx.tx_type() == DEPOSIT_TRANSACTION_TYPE;
        let tx_gas_limit = tx.gas_limit();
        let is_regolith = ctx.cfg().spec().is_enabled_in(OpSpecId::REGOLITH);

        let instruction_result = frame_result.interpreter_result().result;
        let gas = frame_result.gas_mut();
        let remaining = gas.remaining();
        let refunded = gas.refunded();

        // Spend the gas limit. Gas is reimbursed when the tx returns successfully.
        *gas = Gas::new_spent(tx_gas_limit);

        if instruction_result.is_ok() {
            if !is_deposit || is_regolith {
                gas.erase_cost(remaining);
                gas.record_refund(refunded);
            } else if is_deposit && tx.is_system_transaction() {
                gas.erase_cost(tx_gas_limit);
            }
        } else if instruction_result.is_revert() && (!is_deposit || is_regolith) {
            gas.erase_cost(remaining);
        }
        Ok(())
    }

    fn reimburse_caller(
        &self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        let mut additional_refund = U256::ZERO;

        if evm.ctx().tx().tx_type() != DEPOSIT_TRANSACTION_TYPE
            && !evm.ctx().cfg().is_fee_charge_disabled()
        {
            let spec = evm.ctx().cfg().spec();
            additional_refund = evm.ctx().chain().operator_fee_refund(frame_result.gas(), spec);
        }

        // For EIP-8130 sponsored transactions, refund the payer (not tx.caller()).
        if evm.ctx().tx().tx_type() == EIP8130_TX_TYPE {
            let payer = evm.ctx().tx().eip8130_parts().payer;
            let basefee = evm.ctx().block().basefee() as u128;
            let effective_gas_price = evm.ctx().tx().effective_gas_price(basefee);
            let gas = frame_result.gas();
            let refund_amount = U256::from(
                effective_gas_price
                    .saturating_mul((gas.remaining() + gas.refunded() as u64) as u128),
            ) + additional_refund;
            evm.ctx().journal_mut().load_account_mut(payer)?.incr_balance(refund_amount);
            return Ok(());
        }

        reimburse_caller(evm.ctx(), frame_result.gas(), additional_refund).map_err(From::from)
    }

    fn refund(
        &self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        eip7702_refund: i64,
    ) {
        frame_result.gas_mut().record_refund(eip7702_refund);

        let is_deposit = evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;
        let is_regolith = evm.ctx().cfg().spec().is_enabled_in(OpSpecId::REGOLITH);

        // Prior to Regolith, deposit transactions did not receive gas refunds.
        let is_gas_refund_disabled = is_deposit && !is_regolith;
        if !is_gas_refund_disabled {
            frame_result.gas_mut().set_final_refund(
                evm.ctx().cfg().spec().into_eth_spec().is_enabled_in(SpecId::LONDON),
            );
        }
    }

    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        let is_deposit = evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;

        // Transfer fee to coinbase/beneficiary.
        if is_deposit {
            return Ok(());
        }

        self.mainnet.reward_beneficiary(evm, frame_result)?;
        let basefee = evm.ctx().block().basefee() as u128;

        let ctx = evm.ctx();
        let enveloped = ctx.tx().enveloped_tx().cloned();
        let spec = ctx.cfg().spec();
        let l1_block_info = ctx.chain_mut();

        let Some(enveloped_tx) = &enveloped else {
            return Err(ERROR::from_string(
                "[OPTIMISM] Failed to load enveloped transaction.".into(),
            ));
        };

        let l1_cost = l1_block_info.calculate_tx_l1_cost(enveloped_tx, spec);
        let operator_fee_cost = if spec.is_enabled_in(OpSpecId::ISTHMUS) {
            l1_block_info.operator_fee_charge(
                enveloped_tx,
                U256::from(frame_result.gas().used()),
                spec,
            )
        } else {
            U256::ZERO
        };
        let base_fee_amount = U256::from(basefee.saturating_mul(frame_result.gas().used() as u128));

        // Send fees to their respective recipients
        for (recipient, amount) in [
            (L1_FEE_RECIPIENT, l1_cost),
            (BASE_FEE_RECIPIENT, base_fee_amount),
            (OPERATOR_FEE_RECIPIENT, operator_fee_cost),
        ] {
            ctx.journal_mut().balance_incr(recipient, amount)?;
        }

        Ok(())
    }

    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        match core::mem::replace(evm.ctx().error(), Ok(())) {
            Err(ContextError::Db(e)) => return Err(e.into()),
            Err(ContextError::Custom(e)) => return Err(Self::Error::from_string(e)),
            Ok(_) => (),
        }

        let exec_result =
            post_execution::output(evm.ctx(), frame_result).map_haltreason(OpHaltReason::Base);

        if exec_result.is_halt() {
            let is_deposit = evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;
            if is_deposit && evm.ctx().cfg().spec().is_enabled_in(OpSpecId::REGOLITH) {
                return Err(ERROR::from(OpTransactionError::HaltedDepositPostRegolith));
            }
        }
        evm.ctx().journal_mut().commit_tx();
        evm.ctx().chain_mut().clear_tx_l1_cost();
        evm.ctx().local_mut().clear();
        evm.frame_stack().clear();

        Ok(exec_result)
    }

    fn catch_error(
        &self,
        evm: &mut Self::Evm,
        error: Self::Error,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        let is_deposit = evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;
        let is_tx_error = error.is_tx_error();
        let mut output = Err(error);

        // Deposit transaction can't fail so we manually handle it here.
        if is_tx_error && is_deposit {
            let ctx = evm.ctx();
            let spec = ctx.cfg().spec();
            let tx = ctx.tx();
            let caller = tx.caller();
            let mint = tx.mint();
            let is_system_tx = tx.is_system_transaction();
            let gas_limit = tx.gas_limit();
            let journal = evm.ctx().journal_mut();

            // discard all changes of this transaction
            // Default JournalCheckpoint is the first checkpoint and will wipe all changes.
            journal.checkpoint_revert(JournalCheckpoint::default());

            let mut acc = journal.load_account_mut(caller)?;
            acc.bump_nonce();
            acc.incr_balance(U256::from(mint.unwrap_or_default()));

            drop(acc); // Drop acc to avoid borrow checker issues.

            // We can now commit the changes.
            journal.commit_tx();

            let gas_used =
                if spec.is_enabled_in(OpSpecId::REGOLITH) || !is_system_tx { gas_limit } else { 0 };
            // clear the journal
            output = Ok(ExecutionResult::Halt { reason: OpHaltReason::FailedDeposit, gas_used })
        }

        // do the cleanup
        evm.ctx().chain_mut().clear_tx_l1_cost();
        evm.ctx().local_mut().clear();
        evm.frame_stack().clear();

        output
    }
}

impl<EVM, ERROR> InspectorHandler for OpHandler<EVM, ERROR, EthFrame<EthInterpreter>>
where
    EVM: InspectorEvmTr<
            Context: OpContextTr,
            Frame = EthFrame<EthInterpreter>,
            Inspector: Inspector<<<Self as Handler>::Evm as EvmTr>::Context, EthInterpreter>,
        >,
    ERROR: EvmTrError<EVM> + From<OpTransactionError> + FromStringError + IsTxError,
{
    type IT = EthInterpreter;
}

#[cfg(test)]
mod tests {

    use alloy_primitives::uint;
    use revm::{
        bytecode::Bytecode,
        context::{BlockEnv, CfgEnv, Context, TxEnv},
        database::InMemoryDB,
        database_interface::EmptyDB,
        handler::{EthFrame, Handler},
        interpreter::{CallOutcome, InstructionResult, InterpreterResult},
        primitives::{Address, B256, Bytes, TxKind, bytes, hardfork::SpecId},
        state::AccountInfo,
    };

    use super::*;
    use crate::{
        DefaultOp, OpBuilder, OpContext, OpTransaction,
        constants::{
            BASE_FEE_SCALAR_OFFSET, ECOTONE_L1_BLOB_BASE_FEE_SLOT, ECOTONE_L1_FEE_SCALARS_SLOT,
            L1_BASE_FEE_SLOT, L1_BLOCK_CONTRACT, OPERATOR_FEE_SCALARS_SLOT,
        },
    };

    /// Creates frame result.
    fn call_last_frame_return(
        ctx: OpContext<EmptyDB>,
        instruction_result: InstructionResult,
        gas: Gas,
    ) -> Gas {
        let mut evm = ctx.build_op();

        let mut exec_result = FrameResult::Call(CallOutcome::new(
            InterpreterResult { result: instruction_result, output: Bytes::new(), gas },
            0..0,
        ));

        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();

        handler.last_frame_result(&mut evm, &mut exec_result).unwrap();
        handler.refund(&mut evm, &mut exec_result, 0);
        *exec_result.gas()
    }

    #[test]
    fn test_revert_gas() {
        let ctx = Context::op()
            .with_tx(OpTransaction::builder().base(TxEnv::builder().gas_limit(100)).build_fill())
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BEDROCK));

        let gas = call_last_frame_return(ctx, InstructionResult::Revert, Gas::new(90));
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.spent(), 10);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_consume_gas() {
        let ctx = Context::op()
            .with_tx(OpTransaction::builder().base(TxEnv::builder().gas_limit(100)).build_fill())
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::REGOLITH));

        let gas = call_last_frame_return(ctx, InstructionResult::Stop, Gas::new(90));
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.spent(), 10);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_consume_gas_with_refund() {
        let ctx = Context::op()
            .with_tx(
                OpTransaction::builder()
                    .base(TxEnv::builder().gas_limit(100))
                    .source_hash(B256::from([1u8; 32]))
                    .build_fill(),
            )
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::REGOLITH));

        let mut ret_gas = Gas::new(90);
        ret_gas.record_refund(20);

        let gas = call_last_frame_return(ctx.clone(), InstructionResult::Stop, ret_gas);
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.spent(), 10);
        assert_eq!(gas.refunded(), 2); // min(20, 10/5)

        let gas = call_last_frame_return(ctx, InstructionResult::Revert, ret_gas);
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.spent(), 10);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_consume_gas_deposit_tx() {
        let ctx = Context::op()
            .with_tx(
                OpTransaction::builder()
                    .base(TxEnv::builder().gas_limit(100))
                    .source_hash(B256::from([1u8; 32]))
                    .build_fill(),
            )
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BEDROCK));
        let gas = call_last_frame_return(ctx, InstructionResult::Stop, Gas::new(90));
        assert_eq!(gas.remaining(), 0);
        assert_eq!(gas.spent(), 100);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_consume_gas_sys_deposit_tx() {
        let ctx = Context::op()
            .with_tx(
                OpTransaction::builder()
                    .base(TxEnv::builder().gas_limit(100))
                    .source_hash(B256::from([1u8; 32]))
                    .is_system_transaction()
                    .build_fill(),
            )
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BEDROCK));
        let gas = call_last_frame_return(ctx, InstructionResult::Stop, Gas::new(90));
        assert_eq!(gas.remaining(), 100);
        assert_eq!(gas.spent(), 0);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_commit_mint_value() {
        let caller = Address::ZERO;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo { balance: U256::from(1000), ..Default::default() },
        );

        let mut ctx = Context::op()
            .with_db(db)
            .with_chain(L1BlockInfo {
                l1_base_fee: U256::from(1_000),
                l1_fee_overhead: Some(U256::from(1_000)),
                l1_base_fee_scalar: U256::from(1_000),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::REGOLITH));
        ctx.modify_tx(|tx| {
            tx.deposit.source_hash = B256::from([1u8; 32]);
            tx.deposit.mint = Some(10);
        });

        let mut evm = ctx.build_op();

        let handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        handler.validate_against_state_and_deduct_caller(&mut evm).unwrap();

        // Check the account balance is updated.
        let account = evm.ctx().journal_mut().load_account(caller).unwrap();
        assert_eq!(account.info.balance, U256::from(1010));
    }

    #[test]
    fn test_remove_l1_cost_non_deposit() {
        let caller = Address::ZERO;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(1058), // Increased to cover L1 fees (1048) + base fees
                ..Default::default()
            },
        );
        let ctx = Context::op()
            .with_db(db)
            .with_chain(L1BlockInfo {
                l1_base_fee: U256::from(1_000),
                l1_fee_overhead: Some(U256::from(1_000)),
                l1_base_fee_scalar: U256::from(1_000),
                l2_block: Some(U256::from(0)),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::REGOLITH))
            .with_tx(
                OpTransaction::builder()
                    .base(TxEnv::builder().gas_limit(100))
                    .enveloped_tx(Some(bytes!("FACADE")))
                    .source_hash(B256::ZERO)
                    .build()
                    .unwrap(),
            );

        let mut evm = ctx.build_op();

        let handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        handler.validate_against_state_and_deduct_caller(&mut evm).unwrap();

        // Check the account balance is updated.
        let account = evm.ctx().journal_mut().load_account(caller).unwrap();
        assert_eq!(account.info.balance, U256::from(10)); // 1058 - 1048 = 10
    }

    #[test]
    fn test_reload_l1_block_info_isthmus() {
        const BLOCK_NUM: U256 = uint!(100_U256);
        const L1_BASE_FEE: U256 = uint!(1_U256);
        const L1_BLOB_BASE_FEE: U256 = uint!(2_U256);
        const L1_BASE_FEE_SCALAR: u64 = 3;
        const L1_BLOB_BASE_FEE_SCALAR: u64 = 4;
        const L1_FEE_SCALARS: U256 = U256::from_limbs([
            0,
            (L1_BASE_FEE_SCALAR << (64 - BASE_FEE_SCALAR_OFFSET * 2)) | L1_BLOB_BASE_FEE_SCALAR,
            0,
            0,
        ]);
        const OPERATOR_FEE_SCALAR: u64 = 5;
        const OPERATOR_FEE_CONST: u64 = 6;
        const OPERATOR_FEE: U256 =
            U256::from_limbs([OPERATOR_FEE_CONST, OPERATOR_FEE_SCALAR, 0, 0]);

        let mut db = InMemoryDB::default();
        let l1_block_contract = db.load_account(L1_BLOCK_CONTRACT).unwrap();
        l1_block_contract.storage.insert(L1_BASE_FEE_SLOT, L1_BASE_FEE);
        l1_block_contract.storage.insert(ECOTONE_L1_BLOB_BASE_FEE_SLOT, L1_BLOB_BASE_FEE);
        l1_block_contract.storage.insert(ECOTONE_L1_FEE_SCALARS_SLOT, L1_FEE_SCALARS);
        l1_block_contract.storage.insert(OPERATOR_FEE_SCALARS_SLOT, OPERATOR_FEE);
        db.insert_account_info(
            Address::ZERO,
            AccountInfo { balance: U256::from(1000), ..Default::default() },
        );

        let ctx = Context::op()
            .with_db(db)
            .with_chain(L1BlockInfo {
                l2_block: Some(BLOCK_NUM + U256::from(1)), // ahead by one block
                ..Default::default()
            })
            .with_block(BlockEnv { number: BLOCK_NUM, ..Default::default() })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::ISTHMUS));

        let mut evm = ctx.build_op();

        assert_ne!(evm.ctx().chain().l2_block, Some(BLOCK_NUM));

        let handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        handler.validate_against_state_and_deduct_caller(&mut evm).unwrap();

        assert_eq!(
            *evm.ctx().chain(),
            L1BlockInfo {
                l2_block: Some(BLOCK_NUM),
                l1_base_fee: L1_BASE_FEE,
                l1_base_fee_scalar: U256::from(L1_BASE_FEE_SCALAR),
                l1_blob_base_fee: Some(L1_BLOB_BASE_FEE),
                l1_blob_base_fee_scalar: Some(U256::from(L1_BLOB_BASE_FEE_SCALAR)),
                empty_ecotone_scalars: false,
                l1_fee_overhead: None,
                operator_fee_scalar: Some(U256::from(OPERATOR_FEE_SCALAR)),
                operator_fee_constant: Some(U256::from(OPERATOR_FEE_CONST)),
                tx_l1_cost: Some(U256::ZERO),
                da_footprint_gas_scalar: None
            }
        );
    }

    #[test]
    fn test_base_v1_tx_gas_limit_cap_rejected() {
        let ctx = Context::op()
            .with_tx(
                OpTransaction::builder()
                    .base(TxEnv::builder().gas_limit(16_777_217))
                    .enveloped_tx(Some(bytes!("FACADE")))
                    .build_fill(),
            )
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();
        let handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.validate_env(&mut evm);
        assert!(result.is_err(), "gas_limit above cap should be rejected");
    }

    #[test]
    fn test_base_v1_tx_gas_limit_at_cap_ok() {
        let ctx = Context::op()
            .with_tx(
                OpTransaction::builder()
                    .base(TxEnv::builder().gas_limit(16_777_216))
                    .enveloped_tx(Some(bytes!("FACADE")))
                    .build_fill(),
            )
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();
        let handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.validate_env(&mut evm);
        assert!(result.is_ok(), "gas_limit at cap should be accepted");
    }

    #[test]
    fn test_jovian_no_tx_gas_limit_cap() {
        let ctx = Context::op()
            .with_tx(
                OpTransaction::builder()
                    .base(TxEnv::builder().gas_limit(16_777_217))
                    .enveloped_tx(Some(bytes!("FACADE")))
                    .build_fill(),
            )
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::JOVIAN));
        let mut evm = ctx.build_op();
        let handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.validate_env(&mut evm);
        assert!(result.is_ok(), "Jovian should not enforce gas limit cap");
    }

    #[test]
    fn test_base_v1_deposit_skips_gas_limit_cap() {
        let ctx = Context::op()
            .with_tx(
                OpTransaction::builder()
                    .base(TxEnv::builder().gas_limit(16_777_217))
                    .source_hash(B256::from([1u8; 32]))
                    .build_fill(),
            )
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();
        let handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.validate_env(&mut evm);
        assert!(result.is_ok(), "deposit txs should skip gas limit cap");
    }

    #[test]
    fn test_osaka_opcodes_activated_base_v1() {
        assert_eq!(OpSpecId::BASE_V1.into_eth_spec(), SpecId::OSAKA);
    }

    /// Runs CLZ bytecode (`PUSH1 0x80, CLZ, PUSH1 0x00, MSTORE, PUSH1 0x20, PUSH1 0x00, RETURN`)
    /// against the given spec and returns the execution result.
    fn run_clz_bytecode(
        spec: OpSpecId,
    ) -> revm::context_interface::result::ExecutionResult<OpHaltReason> {
        let contract = Address::from([0x42; 20]);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            contract,
            AccountInfo {
                code: Some(Bytecode::new_legacy(bytes!("60801E60005260206000F3"))),
                ..Default::default()
            },
        );
        db.insert_account_info(
            Address::ZERO,
            AccountInfo { balance: U256::from(1_000_000), ..Default::default() },
        );

        let ctx = Context::op()
            .with_db(db)
            .with_tx(
                OpTransaction::builder()
                    .base(TxEnv::builder().gas_limit(100_000).kind(TxKind::Call(contract)))
                    .enveloped_tx(Some(bytes!("FACADE")))
                    .build_fill(),
            )
            .with_cfg(CfgEnv::new_with_spec(spec))
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            });
        let mut evm = ctx.build_op();

        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        handler.run(&mut evm).unwrap()
    }

    #[test]
    fn test_clz_opcode_base_v1() {
        let result = run_clz_bytecode(OpSpecId::BASE_V1);
        assert!(result.is_success(), "CLZ opcode should execute successfully on BASE_V1");

        let output = result.output().unwrap();
        let expected = U256::from(248);
        let actual = U256::from_be_slice(output);
        assert_eq!(actual, expected, "CLZ of 0x80 in 256-bit should be 248");
    }

    #[test]
    fn test_clz_opcode_not_on_jovian() {
        let result = run_clz_bytecode(OpSpecId::JOVIAN);
        assert!(!result.is_success(), "CLZ opcode should not be available on JOVIAN (pre-OSAKA)");
    }

    // -----------------------------------------------------------------------
    // EIP-8130 handler execution tests
    // -----------------------------------------------------------------------

    use crate::{
        Eip8130AuthorizerValidation, Eip8130Call, Eip8130CodePlacement, Eip8130ConfigOp,
        Eip8130Parts, Eip8130SequenceUpdate, Eip8130StorageWrite, Eip8130VerifyCall,
        decode_phase_statuses,
    };

    /// Builds an EVM with EIP-8130 parts and runs the full handler flow,
    /// returning the execution result.
    fn run_eip8130_tx(
        sender: Address,
        accounts: &[(Address, Bytecode)],
        eip8130: Eip8130Parts,
        gas_limit: u64,
    ) -> ExecutionResult<OpHaltReason> {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
        );
        db.insert_account_info(
            NONCE_MANAGER_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );
        for (addr, code) in accounts {
            db.insert_account_info(
                *addr,
                AccountInfo { code: Some(code.clone()), ..Default::default() },
            );
        }

        let mut tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .tx_type(Some(0x7B))
                    .caller(sender)
                    .gas_limit(gas_limit)
                    .kind(TxKind::Call(sender)),
            )
            .enveloped_tx(Some(bytes!("7BFACADE")))
            .build_fill();
        tx.eip8130 = eip8130;

        let ctx = Context::op()
            .with_db(db)
            .with_tx(tx)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();

        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        handler.run(&mut evm).unwrap()
    }

    #[test]
    fn test_eip8130_empty_phases_reverts() {
        let sender = Address::from([0x11; 20]);
        let result = run_eip8130_tx(
            sender,
            &[],
            Eip8130Parts { sender, payer: sender, ..Default::default() },
            100_000,
        );
        assert!(!result.is_success(), "empty phases = no successes = tx reverts");
    }

    #[test]
    fn test_eip8130_deploy_only_succeeds() {
        let sender = Address::from([0x11; 20]);
        let deployed_addr = Address::from([0x99; 20]);
        let bytecode = bytes!("363d3d373d3d3d363d73DEADBEEF5af43d82803e903d91602b57fd5bf3");

        let result = run_eip8130_tx(
            sender,
            &[],
            Eip8130Parts {
                sender,
                payer: sender,
                has_create_entry: true,
                code_placements: vec![Eip8130CodePlacement {
                    address: deployed_addr,
                    code: bytecode,
                }],
                ..Default::default()
            },
            100_000,
        );
        assert!(result.is_success(), "deploy-only tx should succeed");

        let statuses = decode_phase_statuses(result.output().unwrap());
        assert!(statuses.is_empty(), "no call phases = empty statuses");
    }

    #[test]
    fn test_eip8130_single_phase_success() {
        let sender = Address::from([0x11; 20]);
        let target = Address::from([0x22; 20]);

        let result = run_eip8130_tx(
            sender,
            &[(target, Bytecode::new_legacy(bytes!("00")))], // STOP
            Eip8130Parts {
                sender,
                payer: sender,
                call_phases: vec![vec![Eip8130Call {
                    to: target,
                    data: Bytes::new(),
                    value: U256::ZERO,
                }]],
                ..Default::default()
            },
            100_000,
        );
        assert!(result.is_success(), "single STOP call should succeed");

        let statuses = decode_phase_statuses(result.output().unwrap());
        assert_eq!(statuses, vec![true]);
    }

    #[test]
    fn test_eip8130_single_phase_failure_reverts_tx() {
        let sender = Address::from([0x11; 20]);
        let target = Address::from([0x22; 20]);

        let result = run_eip8130_tx(
            sender,
            &[(target, Bytecode::new_legacy(bytes!("60006000FD")))], // REVERT
            Eip8130Parts {
                sender,
                payer: sender,
                call_phases: vec![vec![Eip8130Call {
                    to: target,
                    data: Bytes::new(),
                    value: U256::ZERO,
                }]],
                ..Default::default()
            },
            100_000,
        );
        assert!(!result.is_success(), "all phases failed → tx reverts");
    }

    #[test]
    fn test_eip8130_mixed_phases_succeeds() {
        let sender = Address::from([0x11; 20]);
        let target_ok = Address::from([0x22; 20]);
        let target_fail = Address::from([0x33; 20]);

        let result = run_eip8130_tx(
            sender,
            &[
                (target_ok, Bytecode::new_legacy(bytes!("00"))), // STOP
                (target_fail, Bytecode::new_legacy(bytes!("60006000FD"))), // REVERT
            ],
            Eip8130Parts {
                sender,
                payer: sender,
                call_phases: vec![
                    vec![Eip8130Call { to: target_ok, data: Bytes::new(), value: U256::ZERO }],
                    vec![Eip8130Call { to: target_fail, data: Bytes::new(), value: U256::ZERO }],
                ],
                ..Default::default()
            },
            100_000,
        );
        assert!(result.is_success(), "at least one phase succeeded → tx succeeds");

        let statuses = decode_phase_statuses(result.output().unwrap());
        assert_eq!(statuses, vec![true, false]);
    }

    #[test]
    fn test_eip8130_all_phases_fail_reverts_tx() {
        let sender = Address::from([0x11; 20]);
        let target_fail = Address::from([0x33; 20]);

        let result = run_eip8130_tx(
            sender,
            &[(target_fail, Bytecode::new_legacy(bytes!("60006000FD")))], // REVERT
            Eip8130Parts {
                sender,
                payer: sender,
                call_phases: vec![
                    vec![Eip8130Call { to: target_fail, data: Bytes::new(), value: U256::ZERO }],
                    vec![Eip8130Call { to: target_fail, data: Bytes::new(), value: U256::ZERO }],
                ],
                ..Default::default()
            },
            100_000,
        );
        assert!(!result.is_success(), "all phases failed → tx reverts");
    }

    #[test]
    fn test_eip8130_gas_accounting() {
        let sender = Address::from([0x11; 20]);
        let target = Address::from([0x22; 20]);

        let aa_intrinsic = 25_000u64;
        let gas_limit = 100_000u64;
        let result = run_eip8130_tx(
            sender,
            &[(target, Bytecode::new_legacy(bytes!("00")))], // STOP
            Eip8130Parts {
                sender,
                payer: sender,
                aa_intrinsic_gas: aa_intrinsic,
                call_phases: vec![vec![Eip8130Call {
                    to: target,
                    data: Bytes::new(),
                    value: U256::ZERO,
                }]],
                ..Default::default()
            },
            gas_limit,
        );
        assert!(result.is_success());
        assert!(result.gas_used() >= aa_intrinsic, "at least intrinsic gas should be charged");
        assert!(result.gas_used() <= gas_limit, "cannot spend more than limit");
    }

    #[test]
    fn test_eip8130_warm_nonce_reduces_intrinsic_gas() {
        let sender = Address::from([0x11; 20]);
        let target = Address::from([0x22; 20]);
        let nonce_seq: u64 = 5;
        let aa_intrinsic_cold = 40_000u64;
        let gas_limit = 200_000u64;

        let nonce_key = U256::ZERO;
        let slot = aa_nonce_slot(sender, nonce_key);

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
        );
        db.insert_account_info(
            NONCE_MANAGER_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );
        db.insert_account_info(
            target,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("00"))), ..Default::default() },
        );
        db.insert_account_storage(NONCE_MANAGER_ADDRESS, slot, U256::from(nonce_seq)).unwrap();

        let mut tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .tx_type(Some(0x7B))
                    .caller(sender)
                    .gas_limit(gas_limit)
                    .kind(TxKind::Call(sender)),
            )
            .enveloped_tx(Some(bytes!("7BFACADE")))
            .build_fill();
        tx.base.nonce = nonce_seq;
        tx.eip8130 = Eip8130Parts {
            sender,
            payer: sender,
            nonce_key,
            aa_intrinsic_gas: aa_intrinsic_cold,
            call_phases: vec![vec![Eip8130Call {
                to: target,
                data: Bytes::new(),
                value: U256::ZERO,
            }]],
            ..Default::default()
        };

        let ctx = Context::op()
            .with_db(db)
            .with_tx(tx)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();
        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.run(&mut evm).unwrap();

        assert!(result.is_success());
        let warm_intrinsic = aa_intrinsic_cold - NONCE_COLD_WARM_DELTA;
        assert!(
            result.gas_used() >= warm_intrinsic,
            "gas_used ({}) >= warm intrinsic gas ({})",
            result.gas_used(),
            warm_intrinsic,
        );
        assert!(
            result.gas_used() < aa_intrinsic_cold,
            "warm nonce should use less gas ({}) than cold ({})",
            result.gas_used(),
            aa_intrinsic_cold,
        );
    }

    #[test]
    fn test_eip8130_nonce_mismatch_rejected() {
        let sender = Address::from([0x11; 20]);
        let nonce_key = U256::ZERO;
        let slot = aa_nonce_slot(sender, nonce_key);

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
        );
        db.insert_account_info(
            NONCE_MANAGER_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );
        db.insert_account_storage(NONCE_MANAGER_ADDRESS, slot, U256::from(3u64)).unwrap();

        let mut tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .tx_type(Some(0x7B))
                    .caller(sender)
                    .gas_limit(200_000)
                    .kind(TxKind::Call(sender)),
            )
            .enveloped_tx(Some(bytes!("7BFACADE")))
            .build_fill();
        tx.base.nonce = 5; // state has 3, tx says 5 → NonceTooHigh
        tx.eip8130 = Eip8130Parts {
            sender,
            payer: sender,
            nonce_key,
            aa_intrinsic_gas: 25_000,
            ..Default::default()
        };

        let ctx = Context::op()
            .with_db(db)
            .with_tx(tx)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();
        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.run(&mut evm);
        assert!(result.is_err(), "mismatched nonce should reject the transaction");
    }

    #[test]
    fn test_eip8130_expiry_rejected_at_inclusion() {
        let sender = Address::from([0x11; 20]);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
        );
        db.insert_account_info(
            NONCE_MANAGER_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );

        let mut tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .tx_type(Some(0x7B))
                    .caller(sender)
                    .gas_limit(200_000)
                    .kind(TxKind::Call(sender)),
            )
            .enveloped_tx(Some(bytes!("7BFACADE")))
            .build_fill();
        tx.eip8130 = Eip8130Parts {
            sender,
            payer: sender,
            expiry: 10,
            aa_intrinsic_gas: 25_000,
            ..Default::default()
        };

        let ctx = Context::op()
            .with_db(db)
            .with_tx(tx)
            .with_block(BlockEnv { timestamp: U256::from(11), ..Default::default() })
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();
        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.run(&mut evm);
        assert!(result.is_err(), "expired tx should be rejected at inclusion");
    }

    #[test]
    fn test_eip8130_too_many_calls_rejected_at_inclusion() {
        let sender = Address::from([0x11; 20]);
        let target = Address::from([0x22; 20]);

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
        );
        db.insert_account_info(
            NONCE_MANAGER_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );

        let mut tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .tx_type(Some(0x7B))
                    .caller(sender)
                    .gas_limit(500_000)
                    .kind(TxKind::Call(sender)),
            )
            .enveloped_tx(Some(bytes!("7BFACADE")))
            .build_fill();
        tx.eip8130 = Eip8130Parts {
            sender,
            payer: sender,
            aa_intrinsic_gas: 25_000,
            call_phases: vec![vec![
                Eip8130Call {
                    to: target,
                    data: Bytes::new(),
                    value: U256::ZERO,
                };
                crate::constants::MAX_CALLS_PER_TX + 1
            ]],
            ..Default::default()
        };

        let ctx = Context::op()
            .with_db(db)
            .with_tx(tx)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();
        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.run(&mut evm);
        assert!(result.is_err(), "tx with >100 calls should be rejected at inclusion");
    }

    #[test]
    fn test_eip8130_too_many_account_changes_rejected_at_inclusion() {
        let sender = Address::from([0x11; 20]);

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
        );
        db.insert_account_info(
            NONCE_MANAGER_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );

        let mut tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .tx_type(Some(0x7B))
                    .caller(sender)
                    .gas_limit(500_000)
                    .kind(TxKind::Call(sender)),
            )
            .enveloped_tx(Some(bytes!("7BFACADE")))
            .build_fill();
        tx.eip8130 = Eip8130Parts {
            sender,
            payer: sender,
            aa_intrinsic_gas: 25_000,
            account_change_units: crate::constants::MAX_ACCOUNT_CHANGES_PER_TX + 1,
            ..Default::default()
        };

        let ctx = Context::op()
            .with_db(db)
            .with_tx(tx)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();
        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.run(&mut evm);
        assert!(result.is_err(), "tx with >10 account changes should be rejected at inclusion");
    }

    #[test]
    fn test_eip8130_locked_config_change_rejected_at_inclusion() {
        let sender = Address::from([0x11; 20]);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
        );
        db.insert_account_info(
            NONCE_MANAGER_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );
        db.insert_account_info(ACCOUNT_CONFIG_ADDRESS, AccountInfo::default());

        let lock_slot = aa_lock_slot(sender);
        let account_cfg = db.load_account(ACCOUNT_CONFIG_ADDRESS).unwrap();
        account_cfg.storage.insert(lock_slot, pack_lock_state(true));

        let mut tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .tx_type(Some(0x7B))
                    .caller(sender)
                    .gas_limit(200_000)
                    .kind(TxKind::Call(sender)),
            )
            .enveloped_tx(Some(bytes!("7BFACADE")))
            .build_fill();
        tx.eip8130 = Eip8130Parts {
            sender,
            payer: sender,
            aa_intrinsic_gas: 25_000,
            config_writes: vec![Eip8130StorageWrite {
                address: ACCOUNT_CONFIG_ADDRESS,
                slot: U256::from(1),
                value: U256::from(2),
            }],
            ..Default::default()
        };

        let ctx = Context::op()
            .with_db(db)
            .with_tx(tx)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();
        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.run(&mut evm);
        assert!(result.is_err(), "locked account should reject config change inclusion");
    }

    #[test]
    fn test_eip8130_config_sequence_mismatch_rejected_at_inclusion() {
        let sender = Address::from([0x11; 20]);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
        );
        db.insert_account_info(
            NONCE_MANAGER_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );
        db.insert_account_info(ACCOUNT_CONFIG_ADDRESS, AccountInfo::default());

        let lock_slot = aa_lock_slot(sender);
        let seq_slot = U256::from(0x1234_u64);
        let account_cfg = db.load_account(ACCOUNT_CONFIG_ADDRESS).unwrap();
        account_cfg.storage.insert(lock_slot, pack_lock_state(false));
        account_cfg.storage.insert(seq_slot, pack_sequences(0, 5));

        let mut tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .tx_type(Some(0x7B))
                    .caller(sender)
                    .gas_limit(200_000)
                    .kind(TxKind::Call(sender)),
            )
            .enveloped_tx(Some(bytes!("7BFACADE")))
            .build_fill();
        tx.eip8130 = Eip8130Parts {
            sender,
            payer: sender,
            aa_intrinsic_gas: 25_000,
            sequence_updates: vec![Eip8130SequenceUpdate {
                slot: seq_slot,
                is_multichain: false,
                new_value: 3, // tx sequence = 2, but expected local is 5
            }],
            ..Default::default()
        };

        let ctx = Context::op()
            .with_db(db)
            .with_tx(tx)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();
        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.run(&mut evm);
        assert!(result.is_err(), "sequence mismatch should reject config change inclusion");
    }

    #[test]
    fn test_eip8130_sequence_update_preserves_lock_fields_at_inclusion() {
        let sender = Address::from([0x11; 20]);
        let target = Address::from([0x22; 20]);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
        );
        db.insert_account_info(
            NONCE_MANAGER_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );
        db.insert_account_info(
            target,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("00"))), ..Default::default() },
        );
        db.insert_account_info(
            ACCOUNT_CONFIG_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );

        let state_slot = aa_lock_slot(sender);
        let initial = pack_account_state(5, 9, 0, 600);
        let account_cfg = db.load_account(ACCOUNT_CONFIG_ADDRESS).unwrap();
        account_cfg.storage.insert(state_slot, initial);

        let mut tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .tx_type(Some(0x7B))
                    .caller(sender)
                    .gas_limit(200_000)
                    .kind(TxKind::Call(sender)),
            )
            .enveloped_tx(Some(bytes!("7BFACADE")))
            .build_fill();
        tx.eip8130 = Eip8130Parts {
            sender,
            payer: sender,
            aa_intrinsic_gas: 25_000,
            sequence_updates: vec![Eip8130SequenceUpdate {
                slot: state_slot,
                is_multichain: true,
                new_value: 6, // tx sequence = 5, matches initial multichain sequence
            }],
            call_phases: vec![vec![Eip8130Call {
                to: target,
                data: Bytes::new(),
                value: U256::ZERO,
            }]],
            ..Default::default()
        };

        let ctx = Context::op()
            .with_db(db)
            .with_tx(tx)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();
        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.run(&mut evm).expect("tx should execute");
        assert!(result.is_success(), "inclusion should succeed");

        let account = evm
            .ctx()
            .journal_mut()
            .load_account(ACCOUNT_CONFIG_ADDRESS)
            .expect("account config should be loaded");
        let updated = account
            .storage
            .get(&state_slot)
            .expect("account state slot should be present")
            .present_value();
        let bytes = updated.to_be_bytes::<32>();
        let multichain = u64::from_be_bytes(bytes[24..32].try_into().expect("8-byte slice"));
        let local = u64::from_be_bytes(bytes[16..24].try_into().expect("8-byte slice"));
        let mut unlocks_at_bytes = [0u8; 8];
        unlocks_at_bytes[3..8].copy_from_slice(&bytes[11..16]);
        let unlocks_at = u64::from_be_bytes(unlocks_at_bytes);
        let unlock_delay = u16::from_be_bytes([bytes[9], bytes[10]]);

        assert_eq!(multichain, 6, "multichain sequence should increment");
        assert_eq!(local, 9, "local sequence should be preserved");
        assert_eq!(unlocks_at, 0, "unlocksAt should be preserved");
        assert_eq!(unlock_delay, 600, "unlockDelay should be preserved");
    }

    #[test]
    fn test_eip8130_sender_auth_rejected_when_revoked_in_same_tx() {
        let sender = Address::from([0x11; 20]);
        let target = Address::from([0x22; 20]);
        let k1_verifier = K1_VERIFIER_ADDRESS;
        let new_verifier = Address::from([0x44; 20]);
        let revoked_verifier = REVOKED_VERIFIER;

        let mut implicit_owner = [0u8; 32];
        implicit_owner[..20].copy_from_slice(sender.as_slice());
        let eoa_owner_id = B256::from(implicit_owner);
        let new_owner_id = B256::from([0x55; 32]);

        let eoa_owner_slot = aa_owner_config_slot(sender, U256::from_be_bytes(eoa_owner_id.0));
        let new_owner_slot = aa_owner_config_slot(sender, U256::from_be_bytes(new_owner_id.0));

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
        );
        db.insert_account_info(
            NONCE_MANAGER_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );
        db.insert_account_info(
            target,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("00"))), ..Default::default() },
        );
        db.insert_account_info(
            ACCOUNT_CONFIG_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );

        let mut tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .tx_type(Some(0x7B))
                    .caller(sender)
                    .gas_limit(300_000)
                    .kind(TxKind::Call(sender)),
            )
            .enveloped_tx(Some(bytes!("7BFACADE")))
            .build_fill();
        tx.eip8130 = Eip8130Parts {
            sender,
            payer: sender,
            sender_verifier: k1_verifier,
            owner_id: eoa_owner_id,
            call_phases: vec![vec![Eip8130Call {
                to: target,
                data: Bytes::new(),
                value: U256::ZERO,
            }]],
            authorizer_validations: vec![Eip8130AuthorizerValidation {
                verifier: k1_verifier,
                owner_id: eoa_owner_id,
                verify_call: None,
                owner_changes: vec![
                    Eip8130ConfigOp {
                        change_type: 0x01,
                        verifier: new_verifier,
                        owner_id: new_owner_id,
                        scope: 0,
                    },
                    Eip8130ConfigOp {
                        change_type: 0x02,
                        verifier: Address::ZERO,
                        owner_id: eoa_owner_id,
                        scope: 0,
                    },
                ],
            }],
            config_writes: vec![
                Eip8130StorageWrite {
                    address: ACCOUNT_CONFIG_ADDRESS,
                    slot: new_owner_slot,
                    value: pack_owner_config(new_verifier, 0),
                },
                Eip8130StorageWrite {
                    address: ACCOUNT_CONFIG_ADDRESS,
                    slot: eoa_owner_slot,
                    value: pack_owner_config(revoked_verifier, 0),
                },
            ],
            ..Default::default()
        };

        let ctx = Context::op()
            .with_db(db)
            .with_tx(tx)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();

        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.run(&mut evm);
        assert!(
            result.is_err(),
            "sender auth signed by owner revoked in the same tx must be rejected",
        );
    }

    #[test]
    fn test_eip8130_sender_auth_accepts_owner_added_in_same_tx() {
        let sender = Address::from([0x11; 20]);
        let target = Address::from([0x22; 20]);
        let k1_verifier = K1_VERIFIER_ADDRESS;
        let new_verifier = Address::from([0x44; 20]);
        let revoked_verifier = REVOKED_VERIFIER;

        let mut implicit_owner = [0u8; 32];
        implicit_owner[..20].copy_from_slice(sender.as_slice());
        let eoa_owner_id = B256::from(implicit_owner);
        let new_owner_id = B256::from([0x55; 32]);

        let eoa_owner_slot = aa_owner_config_slot(sender, U256::from_be_bytes(eoa_owner_id.0));
        let new_owner_slot = aa_owner_config_slot(sender, U256::from_be_bytes(new_owner_id.0));

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
        );
        db.insert_account_info(
            NONCE_MANAGER_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );
        db.insert_account_info(
            target,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("00"))), ..Default::default() },
        );
        db.insert_account_info(
            ACCOUNT_CONFIG_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );

        let mut tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .tx_type(Some(0x7B))
                    .caller(sender)
                    .gas_limit(300_000)
                    .kind(TxKind::Call(sender)),
            )
            .enveloped_tx(Some(bytes!("7BFACADE")))
            .build_fill();
        tx.eip8130 = Eip8130Parts {
            sender,
            payer: sender,
            sender_verifier: new_verifier,
            owner_id: new_owner_id,
            call_phases: vec![vec![Eip8130Call {
                to: target,
                data: Bytes::new(),
                value: U256::ZERO,
            }]],
            authorizer_validations: vec![Eip8130AuthorizerValidation {
                verifier: k1_verifier,
                owner_id: eoa_owner_id,
                verify_call: None,
                owner_changes: vec![
                    Eip8130ConfigOp {
                        change_type: 0x01,
                        verifier: new_verifier,
                        owner_id: new_owner_id,
                        scope: 0,
                    },
                    Eip8130ConfigOp {
                        change_type: 0x02,
                        verifier: Address::ZERO,
                        owner_id: eoa_owner_id,
                        scope: 0,
                    },
                ],
            }],
            config_writes: vec![
                Eip8130StorageWrite {
                    address: ACCOUNT_CONFIG_ADDRESS,
                    slot: new_owner_slot,
                    value: pack_owner_config(new_verifier, 0),
                },
                Eip8130StorageWrite {
                    address: ACCOUNT_CONFIG_ADDRESS,
                    slot: eoa_owner_slot,
                    value: pack_owner_config(revoked_verifier, 0),
                },
            ],
            ..Default::default()
        };

        let ctx = Context::op()
            .with_db(db)
            .with_tx(tx)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();

        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.run(&mut evm).expect("tx should execute");
        assert!(
            result.is_success(),
            "sender auth signed by owner authorized in the same tx should succeed",
        );
        let statuses = decode_phase_statuses(result.output().unwrap());
        assert_eq!(statuses, vec![true], "single STOP phase should succeed");
    }

    #[test]
    fn test_eip8130_authorizer_native_verifier_field_mismatch_rejected() {
        let sender = Address::from([0x11; 20]);
        let target = Address::from([0x22; 20]);
        let new_verifier = Address::from([0x44; 20]);
        let revoked_verifier = REVOKED_VERIFIER;

        let mut implicit_owner = [0u8; 32];
        implicit_owner[..20].copy_from_slice(sender.as_slice());
        let eoa_owner_id = B256::from(implicit_owner);
        let new_owner_id = B256::from([0x55; 32]);

        let eoa_owner_slot = aa_owner_config_slot(sender, U256::from_be_bytes(eoa_owner_id.0));
        let new_owner_slot = aa_owner_config_slot(sender, U256::from_be_bytes(new_owner_id.0));

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
        );
        db.insert_account_info(
            NONCE_MANAGER_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );
        db.insert_account_info(
            target,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("00"))), ..Default::default() },
        );
        db.insert_account_info(
            ACCOUNT_CONFIG_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );

        let mut tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .tx_type(Some(0x7B))
                    .caller(sender)
                    .gas_limit(300_000)
                    .kind(TxKind::Call(sender)),
            )
            .enveloped_tx(Some(bytes!("7BFACADE")))
            .build_fill();
        tx.eip8130 = Eip8130Parts {
            sender,
            payer: sender,
            sender_verifier: new_verifier,
            owner_id: new_owner_id,
            call_phases: vec![vec![Eip8130Call {
                to: target,
                data: Bytes::new(),
                value: U256::ZERO,
            }]],
            authorizer_validations: vec![Eip8130AuthorizerValidation {
                // Regression guard: if conversion accidentally zeroes this
                // field, inclusion validation must reject.
                verifier: Address::ZERO,
                owner_id: eoa_owner_id,
                verify_call: None,
                owner_changes: vec![
                    Eip8130ConfigOp {
                        change_type: 0x01,
                        verifier: new_verifier,
                        owner_id: new_owner_id,
                        scope: 0,
                    },
                    Eip8130ConfigOp {
                        change_type: 0x02,
                        verifier: Address::ZERO,
                        owner_id: eoa_owner_id,
                        scope: 0,
                    },
                ],
            }],
            config_writes: vec![
                Eip8130StorageWrite {
                    address: ACCOUNT_CONFIG_ADDRESS,
                    slot: new_owner_slot,
                    value: pack_owner_config(new_verifier, 0),
                },
                Eip8130StorageWrite {
                    address: ACCOUNT_CONFIG_ADDRESS,
                    slot: eoa_owner_slot,
                    value: pack_owner_config(revoked_verifier, 0),
                },
            ],
            ..Default::default()
        };

        let ctx = Context::op()
            .with_db(db)
            .with_tx(tx)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();

        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.run(&mut evm);
        assert!(result.is_err(), "authorizer verifier field mismatch must reject inclusion",);
    }

    #[test]
    fn test_eip8130_custom_authorizer_staticcall_succeeds() {
        let sender = Address::from([0x11; 20]);
        let target = Address::from([0x22; 20]);
        let custom_verifier = Address::from([0xAA; 20]);
        let authorizer_owner_id = B256::from([0xBB; 32]);

        let mut implicit_owner = [0u8; 32];
        implicit_owner[..20].copy_from_slice(sender.as_slice());
        let eoa_owner_id = B256::from(implicit_owner);

        let owner_config_slot =
            aa_owner_config_slot(sender, U256::from_be_bytes(authorizer_owner_id.0));

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
        );
        db.insert_account_info(
            NONCE_MANAGER_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );
        db.insert_account_info(
            custom_verifier,
            AccountInfo {
                code: Some(make_verifier_bytecode(authorizer_owner_id)),
                ..Default::default()
            },
        );
        db.insert_account_info(
            target,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("00"))), ..Default::default() },
        );
        db.insert_account_info(ACCOUNT_CONFIG_ADDRESS, AccountInfo::default());
        let acfg = db.load_account(ACCOUNT_CONFIG_ADDRESS).unwrap();
        acfg.storage.insert(owner_config_slot, pack_owner_config(custom_verifier, 0));

        let mut tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .tx_type(Some(0x7B))
                    .caller(sender)
                    .gas_limit(250_000)
                    .kind(TxKind::Call(sender)),
            )
            .enveloped_tx(Some(bytes!("7BFACADE")))
            .build_fill();
        tx.eip8130 = Eip8130Parts {
            sender,
            payer: sender,
            sender_verifier: K1_VERIFIER_ADDRESS,
            owner_id: eoa_owner_id,
            custom_verifier_gas_cap: 100_000,
            call_phases: vec![vec![Eip8130Call {
                to: target,
                data: Bytes::new(),
                value: U256::ZERO,
            }]],
            authorizer_validations: vec![Eip8130AuthorizerValidation {
                verifier: custom_verifier,
                owner_id: B256::ZERO,
                verify_call: Some(Eip8130VerifyCall {
                    verifier: custom_verifier,
                    calldata: Bytes::from(vec![0xCA; 36]),
                    account: sender,
                    required_scope: crate::constants::OWNER_SCOPE_CONFIG,
                }),
                owner_changes: vec![],
            }],
            ..Default::default()
        };

        let ctx = Context::op()
            .with_db(db)
            .with_tx(tx)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();

        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.run(&mut evm).expect("tx should execute");
        assert!(result.is_success(), "custom authorizer STATICCALL should succeed at inclusion",);
    }

    #[test]
    fn test_eip8130_owner_id_visible_through_tx_context() {
        let sender = Address::from([0x11; 20]);
        let target = Address::from([0x44; 20]);
        let owner_id = B256::from([0xAB; 32]);

        // Runtime for OwnerIdProbe:
        // - probe(): reads TxContext.getOwnerId() and stores it at slot 0
        // - lastOwnerId(): returns slot 0
        let probe_runtime = Bytecode::new_legacy(bytes!(
            "608060405234801561000f575f5ffd5b5060043610610034575f3560e01c80634320a6cb14610038578063b74af5a914610056575b5f5ffd5b610040610074565b60405161004d9190610111565b60405180910390f35b61005e610079565b60405161006b9190610111565b60405180910390f35b5f5481565b5f5f61aa0373ffffffffffffffffffffffffffffffffffffffff16631f5072f26040518163ffffffff1660e01b8152600401602060405180830381865afa1580156100c6573d5f5f3e3d5ffd5b505050506040513d601f19601f820116820180604052508101906100ea9190610158565b9050805f819055508091505090565b5f819050919050565b61010b816100f9565b82525050565b5f6020820190506101245f830184610102565b92915050565b5f5ffd5b610137816100f9565b8114610141575f5ffd5b50565b5f815190506101528161012e565b92915050565b5f6020828403121561016d5761016c61012a565b5b5f61017a84828501610144565b9150509291505056fea26469706673582212203ca48096bb84d6eb04b36713b485cfdc832bcb25ec90dc9b384decb8a8ba23ee64736f6c63430008210033"
        ));

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
        );
        db.insert_account_info(
            NONCE_MANAGER_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );
        db.insert_account_info(
            target,
            AccountInfo { code: Some(probe_runtime), ..Default::default() },
        );

        let mut tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .tx_type(Some(0x7B))
                    .caller(sender)
                    .gas_limit(300_000)
                    .kind(TxKind::Call(sender)),
            )
            .enveloped_tx(Some(bytes!("7BFACADE")))
            .build_fill();
        tx.eip8130 = Eip8130Parts {
            sender,
            payer: sender,
            owner_id,
            call_phases: vec![vec![Eip8130Call {
                to: target,
                data: bytes!("b74af5a9"), // probe()
                value: U256::ZERO,
            }]],
            ..Default::default()
        };

        let ctx = Context::op()
            .with_db(db)
            .with_tx(tx)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();

        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.run(&mut evm).unwrap();
        assert!(result.is_success(), "probe call should succeed");

        let account =
            evm.ctx().journal_mut().load_account(target).expect("target account should be loaded");
        let slot = account.storage.get(&U256::ZERO).expect("probe should write slot 0");
        assert_eq!(
            slot.present_value(),
            U256::from_be_bytes(owner_id.0),
            "slot0 should store owner_id"
        );
    }

    // -----------------------------------------------------------------------
    // Custom verifier STATICCALL tests
    // -----------------------------------------------------------------------

    /// Builds bytecode that returns a fixed 32-byte value (owner_id).
    ///
    /// Bytecode: PUSH32 <id> | PUSH1 0 | MSTORE | PUSH1 32 | PUSH1 0 | RETURN
    fn make_verifier_bytecode(owner_id: B256) -> Bytecode {
        let mut code = vec![0x7f]; // PUSH32
        code.extend_from_slice(owner_id.as_slice());
        code.extend_from_slice(&[
            0x60, 0x00, // PUSH1 0
            0x52, // MSTORE
            0x60, 0x20, // PUSH1 32
            0x60, 0x00, // PUSH1 0
            0xF3, // RETURN
        ]);
        Bytecode::new_legacy(Bytes::from(code))
    }

    /// Packs `(verifier_address, scope)` into the 32-byte word format used by
    /// AccountConfig's owner_config mapping.
    fn pack_owner_config(verifier: Address, scope: u8) -> U256 {
        let mut bytes = [0u8; 32];
        bytes[11] = scope;
        bytes[12..32].copy_from_slice(verifier.as_slice());
        U256::from_be_bytes(bytes)
    }

    /// Packs an `AccountState` storage word.
    ///
    /// Layout: `zeros(9) | unlockDelay(2) | unlocksAt(5) | localSequence(8) | multichainSequence(8)`.
    fn pack_account_state(multichain: u64, local: u64, unlocks_at: u64, unlock_delay: u16) -> U256 {
        let mut bytes = [0u8; 32];
        bytes[24..32].copy_from_slice(&multichain.to_be_bytes());
        bytes[16..24].copy_from_slice(&local.to_be_bytes());
        let unlocks_at_bytes = unlocks_at.to_be_bytes();
        bytes[11..16].copy_from_slice(&unlocks_at_bytes[3..8]);
        bytes[9..11].copy_from_slice(&unlock_delay.to_be_bytes());
        U256::from_be_bytes(bytes)
    }

    /// Packs an `AccountState` slot with only the lock-related fields.
    ///
    /// When `locked` is true, sets `unlocksAt = type(uint40).max`.
    fn pack_lock_state(locked: bool) -> U256 {
        let unlocks_at = if locked { (1_u64 << 40) - 1 } else { 0 };
        pack_account_state(0, 0, unlocks_at, 0)
    }

    fn pack_sequences(multichain: u64, local: u64) -> U256 {
        pack_account_state(multichain, local, 0, 0)
    }

    #[test]
    fn test_eip8130_custom_verifier_staticcall_succeeds() {
        let sender = Address::from([0x11; 20]);
        let target = Address::from([0x22; 20]);
        let verifier = Address::from([0xAA; 20]);
        let owner_id = B256::from([0xBB; 32]);

        let owner_config_slot = aa_owner_config_slot(sender, U256::from_be_bytes(owner_id.0));
        let packed_config = pack_owner_config(verifier, 0x00); // scope=0 = unrestricted

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
        );
        db.insert_account_info(
            NONCE_MANAGER_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );
        db.insert_account_info(
            verifier,
            AccountInfo { code: Some(make_verifier_bytecode(owner_id)), ..Default::default() },
        );
        db.insert_account_info(
            target,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("00"))), ..Default::default() },
        );
        db.insert_account_info(ACCOUNT_CONFIG_ADDRESS, AccountInfo::default());
        let acfg = db.load_account(ACCOUNT_CONFIG_ADDRESS).unwrap();
        acfg.storage.insert(owner_config_slot, packed_config);

        let calldata = Bytes::from(vec![0xCA; 36]); // dummy calldata

        let mut tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .tx_type(Some(0x7B))
                    .caller(sender)
                    .gas_limit(200_000)
                    .kind(TxKind::Call(sender)),
            )
            .enveloped_tx(Some(bytes!("7BFACADE")))
            .build_fill();
        tx.eip8130 = Eip8130Parts {
            sender,
            payer: sender,
            custom_verifier_gas_cap: 100_000,
            sender_verifier: Address::ZERO, // custom
            call_phases: vec![vec![Eip8130Call {
                to: target,
                data: Bytes::new(),
                value: U256::ZERO,
            }]],
            sender_verify_call: Some(Eip8130VerifyCall {
                verifier,
                calldata,
                account: sender,
                required_scope: 0x02, // SENDER
            }),
            ..Default::default()
        };

        let ctx = Context::op()
            .with_db(db)
            .with_tx(tx)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();

        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.run(&mut evm).unwrap();
        assert!(result.is_success(), "custom verifier STATICCALL should succeed");

        let statuses = decode_phase_statuses(result.output().unwrap());
        assert_eq!(statuses, vec![true]);
    }

    #[test]
    fn test_eip8130_custom_verifier_wrong_verifier_fails() {
        let sender = Address::from([0x11; 20]);
        let verifier = Address::from([0xAA; 20]);
        let wrong_verifier = Address::from([0xCC; 20]); // different from expected
        let owner_id = B256::from([0xBB; 32]);

        let owner_config_slot = aa_owner_config_slot(sender, U256::from_be_bytes(owner_id.0));
        // Store a DIFFERENT verifier in owner_config than what the tx specifies
        let packed_config = pack_owner_config(wrong_verifier, 0x00);

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
        );
        db.insert_account_info(
            NONCE_MANAGER_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );
        db.insert_account_info(
            verifier,
            AccountInfo { code: Some(make_verifier_bytecode(owner_id)), ..Default::default() },
        );
        db.insert_account_info(ACCOUNT_CONFIG_ADDRESS, AccountInfo::default());
        let acfg = db.load_account(ACCOUNT_CONFIG_ADDRESS).unwrap();
        acfg.storage.insert(owner_config_slot, packed_config);

        let calldata = Bytes::from(vec![0xCA; 36]);

        let mut tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .tx_type(Some(0x7B))
                    .caller(sender)
                    .gas_limit(200_000)
                    .kind(TxKind::Call(sender)),
            )
            .enveloped_tx(Some(bytes!("7BFACADE")))
            .build_fill();
        tx.eip8130 = Eip8130Parts {
            sender,
            payer: sender,
            custom_verifier_gas_cap: 100_000,
            sender_verifier: Address::ZERO,
            call_phases: vec![],
            sender_verify_call: Some(Eip8130VerifyCall {
                verifier,
                calldata,
                account: sender,
                required_scope: 0x02,
            }),
            ..Default::default()
        };

        let ctx = Context::op()
            .with_db(db)
            .with_tx(tx)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();

        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.run(&mut evm);
        assert!(result.is_err(), "mismatched verifier should cause an error");
    }

    #[test]
    fn test_eip8130_custom_verifier_wrong_scope_fails() {
        let sender = Address::from([0x11; 20]);
        let verifier = Address::from([0xAA; 20]);
        let owner_id = B256::from([0xBB; 32]);

        let owner_config_slot = aa_owner_config_slot(sender, U256::from_be_bytes(owner_id.0));
        // Scope = PAYER (0x04), but required is SENDER (0x02) → should fail
        let packed_config = pack_owner_config(verifier, 0x04);

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
        );
        db.insert_account_info(
            NONCE_MANAGER_ADDRESS,
            AccountInfo { code: Some(Bytecode::new_legacy(bytes!("FE"))), ..Default::default() },
        );
        db.insert_account_info(
            verifier,
            AccountInfo { code: Some(make_verifier_bytecode(owner_id)), ..Default::default() },
        );
        db.insert_account_info(ACCOUNT_CONFIG_ADDRESS, AccountInfo::default());
        let acfg = db.load_account(ACCOUNT_CONFIG_ADDRESS).unwrap();
        acfg.storage.insert(owner_config_slot, packed_config);

        let calldata = Bytes::from(vec![0xCA; 36]);

        let mut tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .tx_type(Some(0x7B))
                    .caller(sender)
                    .gas_limit(200_000)
                    .kind(TxKind::Call(sender)),
            )
            .enveloped_tx(Some(bytes!("7BFACADE")))
            .build_fill();
        tx.eip8130 = Eip8130Parts {
            sender,
            payer: sender,
            custom_verifier_gas_cap: 100_000,
            sender_verifier: Address::ZERO,
            call_phases: vec![],
            sender_verify_call: Some(Eip8130VerifyCall {
                verifier,
                calldata,
                account: sender,
                required_scope: 0x02, // SENDER
            }),
            ..Default::default()
        };

        let ctx = Context::op()
            .with_db(db)
            .with_tx(tx)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BASE_V1));
        let mut evm = ctx.build_op();

        let mut handler =
            OpHandler::<_, EVMError<_, OpTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.run(&mut evm);
        assert!(result.is_err(), "wrong scope should cause an error");
    }
}
