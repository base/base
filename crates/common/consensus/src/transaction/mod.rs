//! Transaction types for Base chains.

mod deposit;
pub use deposit::{DepositTransaction, TxDeposit};

mod eip8130;
pub use eip8130::{
    AA_BASE_COST, AA_PAYER_TYPE, AA_TX_TYPE_ID, ACCOUNT_CONFIG_ADDRESS, ACCOUNT_STATE_BASE_SLOT,
    AccountChangeEntry, AccountState, BYTECODE_BASE_GAS, BYTECODE_PER_BYTE_GAS, CHANGE_TYPE_CONFIG,
    CHANGE_TYPE_CREATE, CHANGE_TYPE_DELEGATION, CONFIG_CHANGE_OP_GAS, CONFIG_CHANGE_SKIP_GAS,
    CUSTOM_VERIFIER_GAS_CAP, Call, CallTuple, ConfigChangeEntry, ConfigOpTuple, CreateEntry,
    DEFAULT_ACCOUNT_ADDRESS, DEFAULT_HIGH_RATE_ACCOUNT_ADDRESS, DELEGATE_VERIFIER_ADDRESS,
    DEPLOYMENT_HEADER_SIZE, DelegationEntry, EOA_AUTH_GAS, EXPIRING_NONCE_GAS,
    EXPIRING_NONCE_SET_CAPACITY, EXPIRING_RING_BASE_SLOT, EXPIRING_RING_PTR_SLOT,
    EXPIRING_SEEN_BASE_SLOT, EXTERNAL_CALLER_VERIFIER, IAccountConfig, INonceManager, ITxContext,
    IVerifier, K1_VERIFIER_ADDRESS, LOCK_BASE_SLOT, MAX_ACCOUNT_CHANGES_PER_TX, MAX_CALLS_PER_TX,
    MAX_CONFIG_OPS_PER_TX, MAX_SIGNATURE_SIZE, NONCE_BASE_SLOT, NONCE_FREE_MAX_EXPIRY_WINDOW,
    NONCE_KEY_COLD_GAS, NONCE_KEY_MAX, NONCE_KEY_WARM_GAS, NONCE_MANAGER_ADDRESS, NativeVerifier,
    OP_AUTHORIZE_OWNER, OP_REVOKE_OWNER, OWNER_CONFIG_BASE_SLOT, Owner, OwnerChange, OwnerScope,
    OwnerTuple, P256_RAW_VERIFIER_ADDRESS, P256_WEBAUTHN_VERIFIER_ADDRESS, ParsedDelegateAuth,
    ParsedSenderAuth, REVOKED_VERIFIER, SEQUENCE_BASE_SLOT, SLOAD_GAS, TX_CONTEXT_ADDRESS,
    TxEip8130, VerifierGasCosts, VerifierKind, account_change_units, account_changes_cost,
    account_state_slot, auth_verifier_kind, authorizer_verification_gas, bytecode_cost,
    config_change_digest, create2_address, delegate_inner_verifier, deployment_code,
    deployment_header, derive_account_address, effective_salt, encode_account_state,
    encode_owner_config, expiring_ring_slot, expiring_seen_slot, intrinsic_gas,
    intrinsic_gas_with_costs, is_account_config_known_deployed, is_native_verifier, lock_slot,
    mark_account_config_deployed, nonce_key_cost, nonce_slot, owner_config_slot,
    parse_account_state, parse_delegate_auth, parse_delegate_data, parse_owner_config,
    parse_sender_auth, payer_auth_cost, payer_signature_hash, payer_verification_gas,
    read_sequence, sender_auth_cost, sender_signature_hash, sender_verification_gas,
    sequence_base_slot, total_verification_gas, tx_payload_cost, verifier_kind, write_sequence,
};
#[cfg(feature = "evm")]
pub use eip8130::{
    CodePlacement, ExecutionCall, LockState, NONCE_MANAGER_GAS, PhaseResult, PrecompileError,
    SequenceUpdateInfo, StorageWrite, TX_CONTEXT_GAS, TxContextValues, ValidationError,
    auto_delegation_code, build_execution_calls, check_lock_state, check_payer_authorization,
    check_sender_authorization, config_change_sequence, config_change_writes, decode_verify_return,
    encode_verify_call, gas_refund, handle_nonce_manager, handle_tx_context, implicit_eoa_owner_id,
    increment_nonce_op, is_owner_authorized, max_execution_gas_cost, nonce_increment_write,
    owner_registration_writes, read_change_sequence, read_lock_state, read_nonce,
    read_owner_config, resolve_sender, validate_config_change_sequences, validate_expiry,
    validate_nonce, validate_structure, write_owner_config_op,
};
#[cfg(feature = "native-verifier")]
pub use eip8130::{NativeVerifyError, NativeVerifyResult, try_native_verify};
pub use eip8130::{PurityScanner, PurityVerdict, PurityViolation, ViolationCategory};

mod tx_type;
pub use tx_type::DEPOSIT_TX_TYPE_ID;

mod envelope;
pub use envelope::{OpEip8130Transaction, OpTransaction, OpTxEnvelope, OpTxType};

mod typed;
pub use typed::OpTypedTransaction;

mod pooled;
#[cfg(feature = "serde")]
pub use deposit::serde_deposit_tx_rpc;
pub use pooled::OpPooledTransaction;

mod meta;
pub use meta::{OpDepositInfo, OpTransactionInfo};

/// Bincode-compatible serde implementations for transaction types.
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub mod serde_bincode_compat {
    pub use super::{deposit::serde_bincode_compat::TxDeposit, envelope::serde_bincode_compat::*};
}
