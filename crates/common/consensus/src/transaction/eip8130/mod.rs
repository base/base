//! EIP-8130: Account Abstraction by Account Configuration.
//!
//! Defines the AA transaction type (`TxEip8130`), supporting types (calls, owners,
//! account change entries), constants, signature hash computation, intrinsic gas
//! calculation, and CREATE2 address derivation.

mod verifier;
pub use verifier::{NativeVerifier, VerifierKind, auth_verifier_kind, verifier_kind};

mod delegate;
pub use delegate::{
    ParsedDelegateAuth, delegate_inner_verifier, parse_delegate_auth, parse_delegate_data,
};

mod constants;
pub use constants::{
    AA_BASE_COST, AA_PAYER_TYPE, AA_TX_TYPE_ID, BYTECODE_BASE_GAS, BYTECODE_PER_BYTE_GAS,
    CONFIG_CHANGE_OP_GAS, CONFIG_CHANGE_SKIP_GAS, CUSTOM_VERIFIER_GAS_CAP, DEPLOYMENT_HEADER_SIZE,
    EOA_AUTH_GAS, EXPIRING_NONCE_GAS, EXPIRING_NONCE_SET_CAPACITY, MAX_ACCOUNT_CHANGES_PER_TX,
    MAX_CALLS_PER_TX, MAX_CONFIG_OPS_PER_TX, MAX_SIGNATURE_SIZE, NONCE_FREE_MAX_EXPIRY_WINDOW,
    NONCE_KEY_COLD_GAS, NONCE_KEY_MAX, NONCE_KEY_WARM_GAS, SLOAD_GAS, VerifierGasCosts,
};

mod types;
pub use types::{
    AccountChangeEntry, CHANGE_TYPE_CONFIG, CHANGE_TYPE_CREATE, CHANGE_TYPE_DELEGATION, Call,
    ConfigChangeEntry, CreateEntry, DelegationEntry, OP_AUTHORIZE_OWNER, OP_REVOKE_OWNER, Owner,
    OwnerChange, OwnerScope,
};

mod tx;
pub use tx::TxEip8130;

mod signature;
pub use signature::{
    ParsedSenderAuth, config_change_digest, parse_sender_auth, payer_signature_hash,
    sender_signature_hash,
};

mod gas;
pub use gas::{
    account_change_units, account_changes_cost, authorizer_verification_gas, bytecode_cost,
    intrinsic_gas, intrinsic_gas_with_costs, nonce_key_cost, payer_auth_cost,
    payer_verification_gas, sender_auth_cost, sender_verification_gas, total_verification_gas,
    tx_payload_cost,
};

mod address;
pub use address::{
    create2_address, deployment_code, deployment_header, derive_account_address, effective_salt,
};

mod abi;
pub use abi::{
    CallTuple, ConfigOpTuple, IAccountConfig, INonceManager, ITxContext, IVerifier, OwnerTuple,
};

mod predeploys;
pub use predeploys::{
    ACCOUNT_CONFIG_ADDRESS, DEFAULT_ACCOUNT_ADDRESS, DEFAULT_HIGH_RATE_ACCOUNT_ADDRESS,
    DELEGATE_VERIFIER_ADDRESS, EXTERNAL_CALLER_VERIFIER, K1_VERIFIER_ADDRESS,
    NONCE_MANAGER_ADDRESS, P256_RAW_VERIFIER_ADDRESS, P256_WEBAUTHN_VERIFIER_ADDRESS,
    REVOKED_VERIFIER, TX_CONTEXT_ADDRESS, is_account_config_known_deployed, is_native_verifier,
    mark_account_config_deployed,
};

mod storage;
pub use storage::{
    ACCOUNT_STATE_BASE_SLOT, AccountState, EXPIRING_RING_BASE_SLOT, EXPIRING_RING_PTR_SLOT,
    EXPIRING_SEEN_BASE_SLOT, LOCK_BASE_SLOT, NONCE_BASE_SLOT, OWNER_CONFIG_BASE_SLOT,
    SEQUENCE_BASE_SLOT, account_state_slot, encode_account_state, encode_owner_config,
    expiring_ring_slot, expiring_seen_slot, lock_slot, nonce_slot, owner_config_slot,
    parse_account_state, parse_owner_config, read_sequence, sequence_base_slot, write_sequence,
};

#[cfg(feature = "evm")]
mod accessors;
#[cfg(feature = "evm")]
pub use accessors::{
    LockState, increment_nonce_op, is_owner_authorized, read_change_sequence, read_lock_state,
    read_nonce, read_owner_config, write_owner_config_op,
};

#[cfg(feature = "evm")]
mod execution;
#[cfg(feature = "evm")]
pub use execution::{
    CodePlacement, ExecutionCall, PhaseResult, SequenceUpdateInfo, StorageWrite, TxContextValues,
    auto_delegation_code, build_execution_calls, config_change_sequence, config_change_writes,
    gas_refund, max_execution_gas_cost, nonce_increment_write, owner_registration_writes,
};

#[cfg(feature = "evm")]
mod precompiles;
#[cfg(feature = "evm")]
pub use precompiles::{
    NONCE_MANAGER_GAS, PrecompileError, TX_CONTEXT_GAS, handle_nonce_manager, handle_tx_context,
};

#[cfg(feature = "evm")]
mod validation;
#[cfg(feature = "evm")]
pub use validation::{
    ValidationError, check_lock_state, check_payer_authorization, check_sender_authorization,
    decode_verify_return, encode_verify_call, implicit_eoa_owner_id, resolve_sender,
    validate_config_change_sequences, validate_expiry, validate_nonce, validate_structure,
    validate_verifier_allowlist,
};

mod purity;
pub use purity::{PurityScanner, PurityVerdict, PurityViolation, ViolationCategory};

#[cfg(feature = "native-verifier")]
mod native_verifier;
#[cfg(feature = "native-verifier")]
pub use native_verifier::{NativeVerifyError, NativeVerifyResult, try_native_verify};

#[cfg(test)]
mod tests;
