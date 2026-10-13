//! Stateful validation of native BLS transactions.

use alloc::vec::Vec;

use alloy_primitives::{Address, U256};
use base_common_consensus::{BlsConstants, BlsPayload, BlsPayloadError, TxBls};
use revm::Database;

use crate::{BaseTransactionError, BlsCrypto, BlsDatabaseState, BlsGas, BlsRegistry, BlsState};

/// A native BLS transaction that passed [`BlsValidator`] checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlsValidated {
    /// The decoded payload.
    pub payload: BlsPayload,
    /// The exact gas the transaction consumes ([`BlsValidator::intrinsic_gas`]).
    pub intrinsic_gas: u64,
}

/// Reasons a native BLS transaction is invalid against a chain and state.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlsValidationError<E> {
    /// Native BLS transactions only execute on [`BlsConstants::CHAIN_ID`].
    #[error("native BLS transactions are disabled on chain {chain_id}")]
    UnsupportedChain {
        /// The executing chain.
        chain_id: u64,
    },
    /// The transaction is signed for a different chain than the executing one.
    #[error("BLS transaction chain id {tx_chain_id} does not match chain {chain_id}")]
    ChainIdMismatch {
        /// The executing chain.
        chain_id: u64,
        /// The transaction's chain id.
        tx_chain_id: u64,
    },
    /// The payload is not canonically encoded.
    #[error(transparent)]
    Malformed(#[from] BlsPayloadError),
    /// The registered public key is not a valid non-identity subgroup point.
    #[error("invalid BLS public key")]
    InvalidPublicKey,
    /// The proof of possession or aggregate signature is not a valid non-identity subgroup point.
    #[error("invalid BLS signature point")]
    InvalidSignaturePoint,
    /// The proof of possession does not verify for the registered key.
    #[error("invalid BLS proof of possession")]
    InvalidProofOfPossession,
    /// The outer signer already has a registered key; keys are immutable.
    #[error("BLS key already registered for {owner}")]
    AlreadyRegistered {
        /// The outer signer.
        owner: Address,
    },
    /// A transfer sender has no registered key.
    #[error("BLS transfer sender {sender} has no registered key")]
    UnregisteredSender {
        /// The transfer sender.
        sender: Address,
    },
    /// The outer signer (relayer) is also an inner transfer sender.
    #[error("BLS batch relayer {sender} is also a transfer sender")]
    SignerIsInnerSender {
        /// The relayer and sender.
        sender: Address,
    },
    /// A transfer nonce does not equal the sender's account nonce.
    #[error("BLS transfer nonce {nonce} for {sender} does not match account nonce {expected}")]
    NonceMismatch {
        /// The transfer sender.
        sender: Address,
        /// The sender's account nonce.
        expected: u64,
        /// The transfer nonce.
        nonce: u64,
    },
    /// The sender's account nonce cannot be incremented.
    #[error("BLS transfer sender {sender} nonce overflows")]
    NonceOverflow {
        /// The transfer sender.
        sender: Address,
    },
    /// A sender's balance before the batch does not cover its transfer.
    #[error("BLS transfer sender {sender} balance {balance} is below value {value}")]
    InsufficientBalance {
        /// The transfer sender.
        sender: Address,
        /// The sender's balance before the batch.
        balance: U256,
        /// The transfer value.
        value: U256,
    },
    /// A transfer sender has code (a contract or an EIP-7702 delegation).
    #[error("BLS transfer sender {sender} has code")]
    SenderHasCode {
        /// The transfer sender.
        sender: Address,
    },
    /// A transfer recipient has code (a contract or an EIP-7702 delegation).
    #[error("BLS transfer recipient {recipient} has code")]
    RecipientHasCode {
        /// The transfer recipient.
        recipient: Address,
    },
    /// A transfer pays the registry account.
    #[error("BLS transfer recipient is the registry")]
    RecipientIsRegistry,
    /// The registry account has code, so it is not the native registry.
    #[error("BLS registry account has code")]
    RegistryHasCode,
    /// The aggregate signature does not verify over the transfers.
    #[error("invalid BLS aggregate signature")]
    InvalidAggregateSignature,
    /// State access failed.
    #[error("BLS validation state access failed: {0}")]
    Database(E),
}

impl<E: core::fmt::Display> BlsValidationError<E> {
    /// Converts into an EVM error: state access failures stay database errors and every other
    /// reason becomes [`BaseTransactionError::Bls`], which rejects the transaction.
    pub fn into_evm_error<T: From<E> + From<BaseTransactionError>>(self) -> T {
        match self {
            Self::Database(error) => error.into(),
            reason => BaseTransactionError::bls(reason).into(),
        }
    }
}

impl<E> BlsValidationError<E> {
    /// Returns a static label for metrics and logs.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::UnsupportedChain { .. } => "unsupported_chain",
            Self::ChainIdMismatch { .. } => "chain_id_mismatch",
            Self::Malformed(_) => "malformed",
            Self::InvalidPublicKey => "invalid_public_key",
            Self::InvalidSignaturePoint => "invalid_signature_point",
            Self::InvalidProofOfPossession => "invalid_proof_of_possession",
            Self::AlreadyRegistered { .. } => "already_registered",
            Self::UnregisteredSender { .. } => "unregistered_sender",
            Self::SignerIsInnerSender { .. } => "signer_is_inner_sender",
            Self::NonceMismatch { .. } => "nonce_mismatch",
            Self::NonceOverflow { .. } => "nonce_overflow",
            Self::InsufficientBalance { .. } => "insufficient_balance",
            Self::SenderHasCode { .. } => "sender_has_code",
            Self::RecipientHasCode { .. } => "recipient_has_code",
            Self::RecipientIsRegistry => "recipient_is_registry",
            Self::RegistryHasCode => "registry_has_code",
            Self::InvalidAggregateSignature => "invalid_aggregate_signature",
            Self::Database(_) => "database",
        }
    }
}

/// Validates native BLS transactions against a chain and state without mutating it.
///
/// Execution (`BaseHandler`) and transaction-pool admission run the same checks, so the pool
/// cannot admit a transaction execution would reject on the same state.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlsValidator;

impl BlsValidator {
    /// Returns the exact gas `tx` consumes. Pure: depends only on the payload.
    pub fn intrinsic_gas(tx: &TxBls) -> Result<u64, BlsPayloadError> {
        Ok(BlsGas::intrinsic_gas(&tx.input, &tx.payload()?))
    }

    /// Validates `tx`, signed by the outer `signer`, for execution on `chain_id` against `db`.
    ///
    /// Covers the chain gate, payload decoding, point validation, the proof of possession or
    /// aggregate signature, registry state, inner nonces, pre-batch balances and code-free
    /// accounts. The outer nonce, balance and fee caps are checked like any transaction's and are
    /// not covered here.
    pub fn validate<DB: Database>(
        db: &mut DB,
        chain_id: u64,
        signer: Address,
        tx: &TxBls,
    ) -> Result<BlsValidated, BlsValidationError<DB::Error>> {
        Self::validate_input(&mut BlsDatabaseState(db), chain_id, tx.chain_id, signer, &tx.input)
    }

    /// [`Self::validate`] over any [`BlsState`] and the raw transaction fields.
    pub fn validate_input<S: BlsState>(
        state: &mut S,
        chain_id: u64,
        tx_chain_id: u64,
        signer: Address,
        input: &[u8],
    ) -> Result<BlsValidated, BlsValidationError<S::Error>> {
        if chain_id != BlsConstants::CHAIN_ID {
            return Err(BlsValidationError::UnsupportedChain { chain_id });
        }
        if tx_chain_id != chain_id {
            return Err(BlsValidationError::ChainIdMismatch { chain_id, tx_chain_id });
        }
        let payload = BlsPayload::decode(input)?;
        if state
            .basic(BlsConstants::REGISTRY_ADDRESS)
            .map_err(BlsValidationError::Database)?
            .is_some_and(|a| !a.is_empty_code_hash())
        {
            return Err(BlsValidationError::RegistryHasCode);
        }

        match &payload {
            BlsPayload::Registration { pubkey, pop } => {
                if BlsRegistry::read_key(state, signer)
                    .map_err(BlsValidationError::Database)?
                    .is_some()
                {
                    return Err(BlsValidationError::AlreadyRegistered { owner: signer });
                }
                let public_key =
                    BlsCrypto::public_key(pubkey).ok_or(BlsValidationError::InvalidPublicKey)?;
                let pop =
                    BlsCrypto::signature(pop).ok_or(BlsValidationError::InvalidSignaturePoint)?;
                if !BlsCrypto::verify_proof_of_possession(pubkey, public_key, pop) {
                    return Err(BlsValidationError::InvalidProofOfPossession);
                }
            }
            BlsPayload::Batch { transfers, signature } => {
                let mut signers = Vec::with_capacity(transfers.len());
                for transfer in transfers {
                    let sender = transfer.sender;
                    if sender == signer {
                        return Err(BlsValidationError::SignerIsInnerSender { sender });
                    }
                    if transfer.recipient == BlsConstants::REGISTRY_ADDRESS {
                        return Err(BlsValidationError::RecipientIsRegistry);
                    }

                    let sender_account = state
                        .basic(sender)
                        .map_err(BlsValidationError::Database)?
                        .unwrap_or_default();
                    if !sender_account.is_empty_code_hash() {
                        return Err(BlsValidationError::SenderHasCode { sender });
                    }
                    if transfer.nonce != sender_account.nonce {
                        return Err(BlsValidationError::NonceMismatch {
                            sender,
                            expected: sender_account.nonce,
                            nonce: transfer.nonce,
                        });
                    }
                    if transfer.nonce == u64::MAX {
                        return Err(BlsValidationError::NonceOverflow { sender });
                    }
                    // Senders are unique, so each sender's single debit is checked against its
                    // balance before the batch; credits from the batch can never fund a debit.
                    let value = U256::from(transfer.value);
                    if sender_account.balance < value {
                        return Err(BlsValidationError::InsufficientBalance {
                            sender,
                            balance: sender_account.balance,
                            value,
                        });
                    }
                    if state
                        .basic(transfer.recipient)
                        .map_err(BlsValidationError::Database)?
                        .is_some_and(|recipient| !recipient.is_empty_code_hash())
                    {
                        return Err(BlsValidationError::RecipientHasCode {
                            recipient: transfer.recipient,
                        });
                    }

                    let key = BlsRegistry::read_key(state, sender)
                        .map_err(BlsValidationError::Database)?
                        .ok_or(BlsValidationError::UnregisteredSender { sender })?;
                    let public_key =
                        BlsCrypto::public_key(&key).ok_or(BlsValidationError::InvalidPublicKey)?;
                    signers.push((public_key, transfer.signing_digest(chain_id)));
                }
                let signature = BlsCrypto::signature(signature)
                    .ok_or(BlsValidationError::InvalidSignaturePoint)?;
                if !BlsCrypto::aggregate_verify(&signers, signature) {
                    return Err(BlsValidationError::InvalidAggregateSignature);
                }
            }
        }

        let intrinsic_gas = BlsGas::intrinsic_gas(input, &payload);
        Ok(BlsValidated { payload, intrinsic_gas })
    }
}
