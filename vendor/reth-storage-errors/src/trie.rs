//! Database-backed trie root, proof, and witness errors.

use alloc::string::ToString;

use alloy_primitives::B256;
use base_execution_state_types::{SparseStateTrieError, SparseStateTrieErrorKind};
use thiserror::Error;

use crate::{db::DatabaseError, provider::ProviderError};

/// State root errors.
#[derive(Error, Clone, Debug)]
pub enum StateRootError {
    /// Internal database error.
    #[error(transparent)]
    Database(#[from] DatabaseError),
    /// Storage root error.
    #[error(transparent)]
    StorageRootError(#[from] StorageRootError),
    /// Provider error when loading prefix sets
    #[error(transparent)]
    PrefixSetLoadError(#[from] ProviderError),
}

impl From<StateRootError> for ProviderError {
    fn from(value: StateRootError) -> Self {
        match value {
            StateRootError::Database(err)
            | StateRootError::StorageRootError(StorageRootError::Database(err)) => {
                Self::Database(err)
            }
            StateRootError::PrefixSetLoadError(err) => err,
        }
    }
}

/// Storage root error.
#[derive(Error, Clone, Debug)]
pub enum StorageRootError {
    /// Internal database error.
    #[error(transparent)]
    Database(#[from] DatabaseError),
}

impl From<StorageRootError> for DatabaseError {
    fn from(err: StorageRootError) -> Self {
        match err {
            StorageRootError::Database(err) => err,
        }
    }
}

/// State proof errors.
#[derive(Error, Clone, Debug)]
pub enum StateProofError {
    /// Internal database error.
    #[error(transparent)]
    Database(#[from] DatabaseError),
    /// RLP decoding error.
    #[error(transparent)]
    Rlp(#[from] alloy_rlp::Error),
    /// Trie inconsistency detected during proof calculation.
    ///
    /// This occurs when cached trie nodes disagree with the leaf data, causing
    /// proof calculation to be unable to make forward progress.
    #[error("trie inconsistency: {0}")]
    TrieInconsistency(alloc::string::String),
}

impl From<StateProofError> for ProviderError {
    fn from(value: StateProofError) -> Self {
        match value {
            StateProofError::Database(error) => Self::Database(error),
            StateProofError::Rlp(error) => Self::Rlp(error),
            StateProofError::TrieInconsistency(msg) => Self::Database(DatabaseError::Other(msg)),
        }
    }
}

/// Trie witness errors.
#[derive(Error, Debug)]
pub enum TrieWitnessError {
    /// Error gather proofs.
    #[error(transparent)]
    Proof(#[from] StateProofError),
    /// RLP decoding error.
    #[error(transparent)]
    Rlp(#[from] alloy_rlp::Error),
    /// Sparse state trie error.
    #[error(transparent)]
    Sparse(#[from] SparseStateTrieError),
    /// Missing account.
    #[error("missing account {_0}")]
    MissingAccount(B256),
}

impl From<SparseStateTrieErrorKind> for TrieWitnessError {
    fn from(error: SparseStateTrieErrorKind) -> Self {
        Self::Sparse(error.into())
    }
}

impl From<TrieWitnessError> for ProviderError {
    fn from(error: TrieWitnessError) -> Self {
        Self::TrieWitnessError(error.to_string())
    }
}
