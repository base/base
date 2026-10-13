//! Stateful validation and witness verification of native BLS transfers (type `0x7b`).

use alloy_primitives::{Address, B256, FixedBytes};
use base_common_consensus::{BlsConstants, TxBlsTransfer};
use revm::Database;

use crate::{BaseTransactionError, BlsCrypto, BlsDatabaseState, BlsRegistry, BlsState};

/// A native BLS transfer authorized by its sender's registered key: the inputs of one term of a
/// block or chunk aggregate signature check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlsTransferIntent {
    /// The transfer sender, which owns `public_key`.
    pub sender: Address,
    /// The sender's registered compressed G2 public key.
    pub public_key: FixedBytes<96>,
    /// The signing digest of the canonical transfer (`TxBlsTransfer::signing_digest`).
    pub digest: B256,
}

/// Reasons a native BLS transfer is invalid against a chain, state or detached witness.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlsTransferError<E> {
    /// Native BLS transfers only execute on [`BlsConstants::CHAIN_ID`].
    #[error("native BLS transfers are disabled on chain {chain_id}")]
    UnsupportedChain {
        /// The executing chain.
        chain_id: u64,
    },
    /// The transfer is for a different chain than the executing one.
    #[error("BLS transfer chain id {tx_chain_id} does not match chain {chain_id}")]
    ChainIdMismatch {
        /// The executing chain.
        chain_id: u64,
        /// The transfer's chain id.
        tx_chain_id: u64,
    },
    /// The priority fee cap exceeds the fee cap.
    #[error(
        "BLS transfer priority fee {max_priority_fee_per_gas} exceeds max fee {max_fee_per_gas}"
    )]
    PriorityFeeAboveMaxFee {
        /// The transfer's priority fee cap.
        max_priority_fee_per_gas: u128,
        /// The transfer's fee cap.
        max_fee_per_gas: u128,
    },
    /// The transfer pays the registry account.
    #[error("BLS transfer recipient is the registry")]
    RecipientIsRegistry,
    /// The transfer pays a precompile, which would execute instead of receiving a plain transfer.
    #[error("BLS transfer recipient {recipient} is a precompile")]
    RecipientIsPrecompile {
        /// The transfer recipient.
        recipient: Address,
    },
    /// The registry account has code, so it is not the native registry.
    #[error("BLS registry account has code")]
    RegistryHasCode,
    /// The sender has code (a contract or an EIP-7702 delegation).
    #[error("BLS transfer sender {sender} has code")]
    SenderHasCode {
        /// The transfer sender.
        sender: Address,
    },
    /// The recipient has code (a contract or an EIP-7702 delegation).
    #[error("BLS transfer recipient {recipient} has code")]
    RecipientHasCode {
        /// The transfer recipient.
        recipient: Address,
    },
    /// The sender has no registered key.
    #[error("BLS transfer sender {sender} has no registered key")]
    UnregisteredSender {
        /// The transfer sender.
        sender: Address,
    },
    /// The sender's registered key is not a valid non-identity subgroup point.
    #[error("BLS transfer sender {sender} has an invalid registered key")]
    InvalidPublicKey {
        /// The transfer sender.
        sender: Address,
    },
    /// The detached signature is not a canonical non-identity subgroup point.
    #[error("invalid BLS transfer signature point")]
    InvalidSignaturePoint,
    /// The detached signature does not verify for the sender's key over the signing digest.
    #[error("invalid BLS transfer signature")]
    InvalidSignature,
    /// State access failed.
    #[error("BLS transfer state access failed: {0}")]
    Database(E),
}

impl<E> BlsTransferError<E> {
    /// Returns whether the detached witness (signature) is at fault. Every other variant except
    /// [`Self::Database`] is a property of the transaction or the state, independent of the
    /// witness.
    pub const fn is_invalid_witness(&self) -> bool {
        matches!(self, Self::InvalidSignaturePoint | Self::InvalidSignature)
    }

    /// Returns a static label for metrics and logs.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::UnsupportedChain { .. } => "unsupported_chain",
            Self::ChainIdMismatch { .. } => "chain_id_mismatch",
            Self::PriorityFeeAboveMaxFee { .. } => "priority_fee_above_max_fee",
            Self::RecipientIsRegistry => "recipient_is_registry",
            Self::RecipientIsPrecompile { .. } => "recipient_is_precompile",
            Self::RegistryHasCode => "registry_has_code",
            Self::SenderHasCode { .. } => "sender_has_code",
            Self::RecipientHasCode { .. } => "recipient_has_code",
            Self::UnregisteredSender { .. } => "unregistered_sender",
            Self::InvalidPublicKey { .. } => "invalid_public_key",
            Self::InvalidSignaturePoint => "invalid_signature_point",
            Self::InvalidSignature => "invalid_signature",
            Self::Database(_) => "database",
        }
    }
}

impl<E: core::fmt::Display> BlsTransferError<E> {
    /// Converts into an EVM error: state access failures stay database errors and every other
    /// reason becomes [`BaseTransactionError::Bls`], which rejects the transaction.
    pub fn into_evm_error<T: From<E> + From<BaseTransactionError>>(self) -> T {
        match self {
            Self::Database(error) => error.into(),
            reason => BaseTransactionError::bls(reason).into(),
        }
    }
}

/// Validates native BLS transfers (type `0x7b`).
///
/// A transfer executes as an ordinary EOA value transfer from its sender, so execution validates
/// only the state preconditions ([`Self::validate_state`]); its nonce, balance and fees are checked
/// like any transaction's. The detached signature is not part of the canonical transaction: it is
/// verified at pool admission ([`Self::validate`]) and, for blocks, as one aggregate per block or
/// derived chunk.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlsTransferValidator;

impl BlsTransferValidator {
    /// Validates `tx` with its detached `signature` for admission on `chain_id` against `db`.
    ///
    /// Runs the chain, fee-cap and [`Self::validate_state`] checks first and only then decodes
    /// the points and runs the pairing. The account nonce, balance and base fee are not checked;
    /// they remain the ordinary pool checks.
    pub fn validate<DB: Database>(
        db: &mut DB,
        tx: &TxBlsTransfer,
        chain_id: u64,
        signature: &FixedBytes<48>,
    ) -> Result<BlsTransferIntent, BlsTransferError<DB::Error>> {
        if tx.max_priority_fee_per_gas > tx.max_fee_per_gas {
            return Err(BlsTransferError::PriorityFeeAboveMaxFee {
                max_priority_fee_per_gas: tx.max_priority_fee_per_gas,
                max_fee_per_gas: tx.max_fee_per_gas,
            });
        }
        let public_key = Self::validate_state(
            &mut BlsDatabaseState(db),
            chain_id,
            tx.chain_id,
            tx.sender,
            tx.recipient,
        )?;
        let intent =
            BlsTransferIntent { sender: tx.sender, public_key, digest: tx.signing_digest() };

        let key = BlsCrypto::public_key(&public_key.0)
            .ok_or(BlsTransferError::InvalidPublicKey { sender: tx.sender })?;
        let signature =
            BlsCrypto::signature(&signature.0).ok_or(BlsTransferError::InvalidSignaturePoint)?;
        if !BlsCrypto::aggregate_verify(&[(key, intent.digest)], signature) {
            return Err(BlsTransferError::InvalidSignature);
        }
        Ok(intent)
    }

    /// Checks the chain gate and the state preconditions of a transfer from `sender` to
    /// `recipient`, returning the sender's registered compressed public key.
    ///
    /// The registry must be the native code-free account, the recipient must not be the registry,
    /// sender and recipient must be code-free (no contract or EIP-7702 delegation) and the sender
    /// must already have a registered key. Shared by execution and pool admission.
    pub fn validate_state<S: BlsState>(
        state: &mut S,
        chain_id: u64,
        tx_chain_id: u64,
        sender: Address,
        recipient: Address,
    ) -> Result<FixedBytes<96>, BlsTransferError<S::Error>> {
        if chain_id != BlsConstants::CHAIN_ID {
            return Err(BlsTransferError::UnsupportedChain { chain_id });
        }
        if tx_chain_id != chain_id {
            return Err(BlsTransferError::ChainIdMismatch { chain_id, tx_chain_id });
        }
        if recipient == BlsConstants::REGISTRY_ADDRESS {
            return Err(BlsTransferError::RecipientIsRegistry);
        }
        let has_code = |state: &mut S, address| {
            state
                .basic(address)
                .map(|account| account.is_some_and(|account| !account.is_empty_code_hash()))
                .map_err(BlsTransferError::Database)
        };
        if has_code(state, BlsConstants::REGISTRY_ADDRESS)? {
            return Err(BlsTransferError::RegistryHasCode);
        }
        if has_code(state, sender)? {
            return Err(BlsTransferError::SenderHasCode { sender });
        }
        if has_code(state, recipient)? {
            return Err(BlsTransferError::RecipientHasCode { recipient });
        }
        BlsRegistry::read_key(state, sender)
            .map_err(BlsTransferError::Database)?
            .map(FixedBytes)
            .ok_or(BlsTransferError::UnregisteredSender { sender })
    }
}
