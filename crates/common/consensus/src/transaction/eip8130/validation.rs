//! AA transaction validation pipeline.
//!
//! Implements the mempool acceptance flow for EIP-8130 transactions. The
//! pipeline validates structural integrity, resolves authentication, and
//! checks on-chain state (owner configuration, nonces, balances, locks).

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::SolCall;
use revm::database::Database;

use super::{
    OwnerScope,
    accessors::{read_change_sequence, read_lock_state, read_nonce, read_owner_config},
    account_change_units,
    constants::{
        MAX_ACCOUNT_CHANGES_PER_TX, MAX_CALLS_PER_TX, MAX_CONFIG_OPS_PER_TX, MAX_SIGNATURE_SIZE,
        NONCE_KEY_MAX,
    },
    predeploys::{K1_VERIFIER_ADDRESS, REVOKED_VERIFIER},
    tx::TxEip8130,
};

/// Errors that can occur during AA transaction validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    /// The `sender_auth` field is too large.
    #[error("sender_auth exceeds max size ({0} > {MAX_SIGNATURE_SIZE})")]
    SenderAuthTooLarge(usize),

    /// The `payer_auth` field is too large.
    #[error("payer_auth exceeds max size ({0} > {MAX_SIGNATURE_SIZE})")]
    PayerAuthTooLarge(usize),

    /// A config-change `authorizer_auth` field is too large.
    #[error("authorizer_auth exceeds max size ({0} > {MAX_SIGNATURE_SIZE})")]
    AuthorizerAuthTooLarge(usize),

    /// Failed to parse `sender_auth`.
    #[error("invalid sender_auth: {0}")]
    InvalidSenderAuth(&'static str),

    /// Failed to parse `payer_auth`.
    #[error("invalid payer_auth: {0}")]
    InvalidPayerAuth(&'static str),

    /// `account_changes` has invalid structure (e.g. create not first).
    #[error("invalid account_changes structure: {0}")]
    InvalidAccountChanges(&'static str),

    /// The transaction has too many calls across all phases.
    #[error("too many calls in transaction ({count} > {limit})")]
    TooManyCalls {
        /// Number of calls in the transaction.
        count: usize,
        /// Maximum allowed calls.
        limit: usize,
    },

    /// The transaction has too many account-change units.
    #[error("too many account changes in transaction ({count} > {limit})")]
    TooManyAccountChanges {
        /// Number of account-change units in the transaction.
        count: usize,
        /// Maximum allowed account-change units.
        limit: usize,
    },

    /// The transaction has too many config operations across all config changes.
    #[error("too many config operations in transaction ({count} > {limit})")]
    TooManyConfigOperations {
        /// Number of config operations in the transaction.
        count: usize,
        /// Maximum allowed config operations.
        limit: usize,
    },

    /// The transaction has expired.
    #[error("transaction expired (expiry={expiry}, current={current})")]
    Expired {
        /// The transaction's expiry timestamp.
        expiry: u64,
        /// The current block timestamp.
        current: u64,
    },

    /// Nonce mismatch.
    #[error("nonce mismatch (expected={expected}, got={got})")]
    NonceMismatch {
        /// The expected nonce.
        expected: u64,
        /// The nonce in the transaction.
        got: u64,
    },

    /// Nonce-free mode (`nonce_key == NONCE_KEY_MAX`) requires `nonce_sequence == 0`.
    #[error("nonce-free mode requires nonce_sequence == 0")]
    NonceFreeSequenceMustBeZero,

    /// Nonce-free mode requires a non-zero `expiry` for replay protection.
    #[error("nonce-free mode requires non-zero expiry")]
    NonceFreeRequiresExpiry,

    /// The sender's owner is not authorized.
    #[error("sender owner not authorized")]
    SenderNotAuthorized,

    /// The sender's owner lacks the required scope bit.
    #[error("sender owner lacks SENDER scope")]
    SenderScopeMissing,

    /// The payer's owner is not authorized.
    #[error("payer owner not authorized")]
    PayerNotAuthorized,

    /// The payer's owner lacks the required scope bit.
    #[error("payer owner lacks PAYER scope")]
    PayerScopeMissing,

    /// Verifier STATICCALL reverted or returned invalid data.
    #[error("verifier call failed: {0}")]
    VerifierCallFailed(String),

    /// The account is locked and config changes are not allowed.
    #[error("account is locked")]
    AccountLocked,

    /// Config change sequence mismatch.
    #[error("config change sequence mismatch (expected={expected}, got={got})")]
    SequenceMismatch {
        /// The expected sequence.
        expected: u64,
        /// The sequence in the transaction.
        got: u64,
    },

    /// Payer has insufficient balance.
    #[error("payer insufficient balance (required={required}, available={available})")]
    InsufficientBalance {
        /// The required balance.
        required: U256,
        /// The available balance.
        available: U256,
    },

    /// Database error during validation.
    #[error("database error: {0}")]
    Database(String),
}

/// Validates structural constraints that don't require DB access.
pub fn validate_structure(tx: &TxEip8130) -> Result<(), ValidationError> {
    if tx.sender_auth.len() > MAX_SIGNATURE_SIZE {
        return Err(ValidationError::SenderAuthTooLarge(tx.sender_auth.len()));
    }
    if tx.payer_auth.len() > MAX_SIGNATURE_SIZE {
        return Err(ValidationError::PayerAuthTooLarge(tx.payer_auth.len()));
    }

    if tx.nonce_key == NONCE_KEY_MAX {
        if tx.nonce_sequence != 0 {
            return Err(ValidationError::NonceFreeSequenceMustBeZero);
        }
        if tx.expiry == 0 {
            return Err(ValidationError::NonceFreeRequiresExpiry);
        }
    }

    validate_account_changes_structure(tx)?;
    validate_calls_limit(tx)?;
    validate_account_changes_limit(tx)?;
    validate_config_operations_limit(tx)?;
    validate_authorizer_auth_sizes(tx)?;

    Ok(())
}

/// Validates the `account_changes` array structure.
fn validate_account_changes_structure(tx: &TxEip8130) -> Result<(), ValidationError> {
    use super::types::AccountChangeEntry;

    let mut seen_create = false;
    for (i, entry) in tx.account_changes.iter().enumerate() {
        match entry {
            AccountChangeEntry::Create(_) => {
                if seen_create {
                    return Err(ValidationError::InvalidAccountChanges("multiple create entries"));
                }
                if i != 0 {
                    return Err(ValidationError::InvalidAccountChanges(
                        "create entry must be first",
                    ));
                }
                seen_create = true;
            }
            AccountChangeEntry::ConfigChange(cc) => {
                if cc.owner_changes.is_empty() {
                    return Err(ValidationError::InvalidAccountChanges(
                        "config change entry must include at least one operation",
                    ));
                }
            }
            AccountChangeEntry::Delegation(_) => {}
        }
    }
    Ok(())
}

/// Validates the total call count across all phases.
fn validate_calls_limit(tx: &TxEip8130) -> Result<(), ValidationError> {
    let total_calls: usize = tx.calls.iter().map(Vec::len).sum();
    if total_calls > MAX_CALLS_PER_TX {
        return Err(ValidationError::TooManyCalls { count: total_calls, limit: MAX_CALLS_PER_TX });
    }
    Ok(())
}

/// Validates the total account-change units in a transaction.
fn validate_account_changes_limit(tx: &TxEip8130) -> Result<(), ValidationError> {
    let total = account_change_units(tx);
    if total > MAX_ACCOUNT_CHANGES_PER_TX {
        return Err(ValidationError::TooManyAccountChanges {
            count: total,
            limit: MAX_ACCOUNT_CHANGES_PER_TX,
        });
    }
    Ok(())
}

/// Validates total config operation count across all config change entries.
fn validate_config_operations_limit(tx: &TxEip8130) -> Result<(), ValidationError> {
    use super::types::AccountChangeEntry;

    let total_ops: usize = tx
        .account_changes
        .iter()
        .map(|entry| match entry {
            AccountChangeEntry::ConfigChange(cc) => cc.owner_changes.len(),
            AccountChangeEntry::Create(_) | AccountChangeEntry::Delegation(_) => 0,
        })
        .sum();
    if total_ops > MAX_CONFIG_OPS_PER_TX {
        return Err(ValidationError::TooManyConfigOperations {
            count: total_ops,
            limit: MAX_CONFIG_OPS_PER_TX,
        });
    }
    Ok(())
}

/// Validates config-change authorizer auth blobs with the same size bound as sender/payer auth.
fn validate_authorizer_auth_sizes(tx: &TxEip8130) -> Result<(), ValidationError> {
    use super::types::AccountChangeEntry;

    for entry in &tx.account_changes {
        let AccountChangeEntry::ConfigChange(cc) = entry else {
            continue;
        };
        if cc.authorizer_auth.len() > MAX_SIGNATURE_SIZE {
            return Err(ValidationError::AuthorizerAuthTooLarge(cc.authorizer_auth.len()));
        }
    }
    Ok(())
}

/// Validates expiry against the current block timestamp.
pub fn validate_expiry(tx: &TxEip8130, block_timestamp: u64) -> Result<(), ValidationError> {
    if tx.expiry != 0 && block_timestamp > tx.expiry {
        return Err(ValidationError::Expired { expiry: tx.expiry, current: block_timestamp });
    }
    Ok(())
}

/// Validates the nonce against on-chain state.
pub fn validate_nonce<DB: Database>(
    db: &mut DB,
    sender: Address,
    tx: &TxEip8130,
) -> Result<(), ValidationError> {
    let current = read_nonce(db, sender, tx.nonce_key)
        .map_err(|e| ValidationError::Database(format!("{e:?}")))?;
    if current != tx.nonce_sequence {
        return Err(ValidationError::NonceMismatch { expected: current, got: tx.nonce_sequence });
    }
    Ok(())
}

/// Resolves the effective sender address.
///
/// If `from` is empty (EOA mode), a recovered sender address must be
/// provided by ingress recovery. Otherwise, the `from` field is used directly.
pub fn resolve_sender(
    tx: &TxEip8130,
    recovered_sender: Option<Address>,
) -> Result<Address, ValidationError> {
    if tx.is_eoa() {
        return recovered_sender
            .ok_or(ValidationError::InvalidSenderAuth("EOA sender must be recovered at ingress"));
    }
    tx.from.ok_or(ValidationError::InvalidSenderAuth("configured sender must set from field"))
}

/// Checks whether the sender is authorized by verifying their `owner_config`.
///
/// For the implicit EOA rule: if the slot is empty and
/// `owner_id == bytes32(bytes20(account))`, the K1 verifier is used by default.
pub fn check_sender_authorization<DB: Database>(
    db: &mut DB,
    account: Address,
    owner_id: B256,
) -> Result<(Address, u8), ValidationError> {
    let (verifier, scope) = read_owner_config(db, account, owner_id)
        .map_err(|e| ValidationError::Database(format!("{e:?}")))?;

    if verifier == REVOKED_VERIFIER {
        return Err(ValidationError::SenderNotAuthorized);
    }

    if verifier != Address::ZERO {
        if scope != 0 && (scope & OwnerScope::SENDER) == 0 {
            return Err(ValidationError::SenderScopeMissing);
        }
        return Ok((verifier, scope));
    }

    // verifier == address(0): empty slot, implicit EOA rule.
    let implicit_owner_id = implicit_eoa_owner_id(account);
    if owner_id == implicit_owner_id {
        return Ok((K1_VERIFIER_ADDRESS, 0));
    }

    Err(ValidationError::SenderNotAuthorized)
}

/// Checks whether the payer is authorized.
pub fn check_payer_authorization<DB: Database>(
    db: &mut DB,
    payer: Address,
    owner_id: B256,
) -> Result<(Address, u8), ValidationError> {
    let (verifier, scope) = read_owner_config(db, payer, owner_id)
        .map_err(|e| ValidationError::Database(format!("{e:?}")))?;

    if verifier == REVOKED_VERIFIER {
        return Err(ValidationError::PayerNotAuthorized);
    }

    if verifier != Address::ZERO {
        if scope != 0 && (scope & OwnerScope::PAYER) == 0 {
            return Err(ValidationError::PayerScopeMissing);
        }
        return Ok((verifier, scope));
    }

    // verifier == address(0): empty slot, implicit EOA rule.
    let implicit_owner_id = implicit_eoa_owner_id(payer);
    if owner_id == implicit_owner_id {
        return Ok((K1_VERIFIER_ADDRESS, 0));
    }

    Err(ValidationError::PayerNotAuthorized)
}

/// Checks lock state before config changes.
///
/// An account is locked when `block.timestamp < unlocks_at`.
pub fn check_lock_state<DB: Database>(
    db: &mut DB,
    account: Address,
    block_timestamp: u64,
) -> Result<(), ValidationError> {
    let lock =
        read_lock_state(db, account).map_err(|e| ValidationError::Database(format!("{e:?}")))?;
    if block_timestamp < lock.unlocks_at {
        return Err(ValidationError::AccountLocked);
    }
    Ok(())
}

/// Validates config change sequences.
pub fn validate_config_change_sequences<DB: Database>(
    db: &mut DB,
    sender: Address,
    tx: &TxEip8130,
) -> Result<(), ValidationError> {
    use super::types::AccountChangeEntry;

    for entry in &tx.account_changes {
        if let AccountChangeEntry::ConfigChange(change) = entry {
            let expected = read_change_sequence(db, sender, change.chain_id)
                .map_err(|e| ValidationError::Database(format!("{e:?}")))?;
            if change.sequence != expected {
                return Err(ValidationError::SequenceMismatch { expected, got: change.sequence });
            }
        }
    }
    Ok(())
}

/// Computes `bytes32(bytes20(account))` — the implicit EOA owner ID.
pub fn implicit_eoa_owner_id(account: Address) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[..20].copy_from_slice(account.as_slice());
    B256::from(bytes)
}

/// Encodes a STATICCALL to `IVerifier.verify(hash, data)`.
pub fn encode_verify_call(hash: B256, data: &Bytes) -> Bytes {
    use super::abi::IVerifier;
    let call = IVerifier::verifyCall { hash, data: data.clone() };
    Bytes::from(call.abi_encode())
}

/// Decodes the return value from `IVerifier.verify()` → `ownerId`.
pub fn decode_verify_return(output: &[u8]) -> Option<B256> {
    use super::abi::IVerifier;
    IVerifier::verifyCall::abi_decode_returns(output).ok()
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;

    use super::super::constants::NONCE_KEY_MAX;
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn implicit_eoa_owner_id_correct() {
        let account = address!("0x1111111111111111111111111111111111111111");
        let owner_id = implicit_eoa_owner_id(account);
        assert_eq!(&owner_id.as_slice()[..20], account.as_slice());
        assert!(owner_id.as_slice()[20..].iter().all(|&b| b == 0));
    }

    #[test]
    fn structure_validation_empty_tx() {
        let tx = TxEip8130::default();
        assert!(validate_structure(&tx).is_ok());
    }

    #[test]
    fn structure_validation_sender_auth_too_large() {
        let tx = TxEip8130 {
            sender_auth: Bytes::from(vec![0u8; MAX_SIGNATURE_SIZE + 1]),
            ..Default::default()
        };
        assert!(matches!(validate_structure(&tx), Err(ValidationError::SenderAuthTooLarge(_))));
    }

    #[test]
    fn structure_validation_call_count_limit() {
        let calls = (0..MAX_CALLS_PER_TX)
            .map(|_| super::super::types::Call {
                to: address!("0x2222222222222222222222222222222222222222"),
                data: Bytes::new(),
            })
            .collect();
        let tx = TxEip8130 { calls: vec![calls], ..Default::default() };
        assert!(validate_structure(&tx).is_ok());
    }

    #[test]
    fn structure_validation_too_many_calls() {
        let calls = (0..(MAX_CALLS_PER_TX + 1))
            .map(|_| super::super::types::Call {
                to: address!("0x2222222222222222222222222222222222222222"),
                data: Bytes::new(),
            })
            .collect();
        let tx = TxEip8130 { calls: vec![calls], ..Default::default() };
        assert!(matches!(
            validate_structure(&tx),
            Err(ValidationError::TooManyCalls {
                count,
                limit
            }) if count == MAX_CALLS_PER_TX + 1 && limit == MAX_CALLS_PER_TX
        ));
    }

    #[test]
    fn structure_validation_account_changes_limit() {
        let owners = (0..9)
            .map(|i| super::super::types::Owner {
                verifier: address!("0x3333333333333333333333333333333333333333"),
                owner_id: {
                    let mut id = [0u8; 32];
                    id[31] = i as u8;
                    B256::from(id)
                },
                scope: 0,
            })
            .collect();
        let tx = TxEip8130 {
            account_changes: vec![super::super::types::AccountChangeEntry::Create(
                super::super::types::CreateEntry {
                    user_salt: B256::repeat_byte(0xAA),
                    bytecode: Bytes::new(),
                    initial_owners: owners,
                },
            )],
            ..Default::default()
        };
        assert!(validate_structure(&tx).is_ok());
    }

    #[test]
    fn structure_validation_too_many_account_changes() {
        let owners = (0..10)
            .map(|i| super::super::types::Owner {
                verifier: address!("0x3333333333333333333333333333333333333333"),
                owner_id: {
                    let mut id = [0u8; 32];
                    id[31] = i as u8;
                    B256::from(id)
                },
                scope: 0,
            })
            .collect();
        let tx = TxEip8130 {
            account_changes: vec![super::super::types::AccountChangeEntry::Create(
                super::super::types::CreateEntry {
                    user_salt: B256::repeat_byte(0xAA),
                    bytecode: Bytes::new(),
                    initial_owners: owners,
                },
            )],
            ..Default::default()
        };
        assert!(matches!(
            validate_structure(&tx),
            Err(ValidationError::TooManyAccountChanges {
                count,
                limit
            }) if count == MAX_ACCOUNT_CHANGES_PER_TX + 1
                && limit == MAX_ACCOUNT_CHANGES_PER_TX
        ));
    }

    #[test]
    fn structure_validation_rejects_empty_config_change_operations() {
        let tx = TxEip8130 {
            account_changes: vec![super::super::types::AccountChangeEntry::ConfigChange(
                super::super::types::ConfigChangeEntry {
                    chain_id: 8453,
                    sequence: 0,
                    owner_changes: vec![],
                    authorizer_auth: Bytes::new(),
                },
            )],
            ..Default::default()
        };
        assert!(matches!(
            validate_structure(&tx),
            Err(ValidationError::InvalidAccountChanges(
                "config change entry must include at least one operation"
            ))
        ));
    }

    #[test]
    fn structure_validation_rejects_too_many_config_operations() {
        let ops = (0..(MAX_CONFIG_OPS_PER_TX + 1))
            .map(|_| super::super::types::OwnerChange {
                change_type: 0x01,
                verifier: address!("0x5555555555555555555555555555555555555555"),
                owner_id: B256::ZERO,
                scope: 0,
            })
            .collect();
        let tx = TxEip8130 {
            account_changes: vec![super::super::types::AccountChangeEntry::ConfigChange(
                super::super::types::ConfigChangeEntry {
                    chain_id: 8453,
                    sequence: 0,
                    owner_changes: ops,
                    authorizer_auth: Bytes::new(),
                },
            )],
            ..Default::default()
        };
        assert!(matches!(
            validate_structure(&tx),
            Err(ValidationError::TooManyConfigOperations {
                count,
                limit
            }) if count == MAX_CONFIG_OPS_PER_TX + 1 && limit == MAX_CONFIG_OPS_PER_TX
        ));
    }

    #[test]
    fn structure_validation_authorizer_auth_too_large() {
        let tx = TxEip8130 {
            account_changes: vec![super::super::types::AccountChangeEntry::ConfigChange(
                super::super::types::ConfigChangeEntry {
                    chain_id: 8453,
                    sequence: 0,
                    owner_changes: vec![super::super::types::OwnerChange {
                        change_type: 0x01,
                        verifier: address!("0x6666666666666666666666666666666666666666"),
                        owner_id: B256::ZERO,
                        scope: 0,
                    }],
                    authorizer_auth: Bytes::from(vec![0u8; MAX_SIGNATURE_SIZE + 1]),
                },
            )],
            ..Default::default()
        };
        assert!(matches!(
            validate_structure(&tx),
            Err(ValidationError::AuthorizerAuthTooLarge(size))
            if size == MAX_SIGNATURE_SIZE + 1
        ));
    }

    #[test]
    fn resolve_sender_eoa_requires_recovered_sender() {
        let tx = TxEip8130 { from: None, ..Default::default() };
        assert!(matches!(
            resolve_sender(&tx, None),
            Err(ValidationError::InvalidSenderAuth("EOA sender must be recovered at ingress"))
        ));

        let recovered = address!("0x7777777777777777777777777777777777777777");
        assert_eq!(resolve_sender(&tx, Some(recovered)).unwrap(), recovered);
    }

    #[test]
    fn resolve_sender_configured_uses_from() {
        let from = address!("0x8888888888888888888888888888888888888888");
        let tx = TxEip8130 { from: Some(from), ..Default::default() };
        assert_eq!(resolve_sender(&tx, None).unwrap(), from);
        assert_eq!(resolve_sender(&tx, Some(Address::repeat_byte(0x99))).unwrap(), from);
    }

    #[test]
    fn expiry_validation() {
        let tx = TxEip8130 { expiry: 100, ..Default::default() };
        assert!(validate_expiry(&tx, 50).is_ok());
        assert!(validate_expiry(&tx, 100).is_ok());
        assert!(matches!(validate_expiry(&tx, 101), Err(ValidationError::Expired { .. })));
    }

    #[test]
    fn no_expiry_always_valid() {
        let tx = TxEip8130 { expiry: 0, ..Default::default() };
        assert!(validate_expiry(&tx, u64::MAX).is_ok());
    }

    #[test]
    fn encode_decode_verify_roundtrip() {
        let hash = B256::repeat_byte(0xAA);
        let data = Bytes::from(vec![1, 2, 3, 4]);
        let encoded = encode_verify_call(hash, &data);
        assert!(!encoded.is_empty());
    }

    #[test]
    fn structure_validation_nonce_free_requires_zero_sequence() {
        let tx = TxEip8130 {
            nonce_key: NONCE_KEY_MAX,
            nonce_sequence: 1,
            expiry: 100,
            ..Default::default()
        };
        assert!(matches!(
            validate_structure(&tx),
            Err(ValidationError::NonceFreeSequenceMustBeZero)
        ));
    }

    #[test]
    fn structure_validation_nonce_free_requires_expiry() {
        let tx = TxEip8130 {
            nonce_key: NONCE_KEY_MAX,
            nonce_sequence: 0,
            expiry: 0,
            ..Default::default()
        };
        assert!(matches!(validate_structure(&tx), Err(ValidationError::NonceFreeRequiresExpiry)));
    }

    #[test]
    fn structure_validation_nonce_free_valid() {
        let tx = TxEip8130 {
            nonce_key: NONCE_KEY_MAX,
            nonce_sequence: 0,
            expiry: 100,
            ..Default::default()
        };
        assert!(validate_structure(&tx).is_ok());
    }
}
