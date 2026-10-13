//! Block-level authorization of native BLS transfers.

use alloc::vec::Vec;

use alloy_primitives::{Address, FixedBytes};

use crate::{BlsCrypto, BlsTransferIntent};

/// How a block's native BLS transfers (type `0x7b`) are authorized.
///
/// Canonical transfers carry no signature, so ordinary execution cannot tell an authorized
/// transfer from a forged one. Every execution path must therefore either verify a detached
/// aggregate witness for the block ([`Self::Verify`]) or execute on behalf of a path that
/// already authenticated the block's transfers ([`Self::Derived`]).
///
/// This is a per-block execution-context value chosen by the caller. It is never part of a
/// transaction, the serialized DA data or a pool setting, and there is no global or cached
/// "preverified" state: each block execution carries its own authorization. A caller may derive
/// [`Self::Derived`] only from an already-authenticated assertion (for example an authenticated
/// Engine API payload-attributes flag set by the CL for blocks it derived, or a durable
/// per-block marker written by such a path), never from data an untrusted peer supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlsBlockAuthorization {
    /// Verify the block's transfers against this aggregate signature when the block finishes.
    ///
    /// The aggregate must sum one signature per included transfer, each by the sender's
    /// registered key over the transfer's signing digest. A block without transfers requires
    /// `None`; a block with transfers fails closed without a valid aggregate.
    Verify(Option<FixedBytes<48>>),
    /// The block's transfers were already authenticated upstream, so no witness is checked.
    ///
    /// Trust boundary: only for blocks whose transfers were cryptographically verified before
    /// execution by the caller's own path — blocks derived from L1 data after the chunk
    /// aggregate passed `BlsChunkVerifier` (CL derivation and fault/validity proofs), or local
    /// block building from transactions whose witnesses were verified at pool admission. Never
    /// use it for blocks received from peers, the database or an untrusted Engine API caller.
    Derived,
}

impl Default for BlsBlockAuthorization {
    /// Fails closed: verify, with no aggregate, so any transfer makes the block fail.
    fn default() -> Self {
        Self::Verify(None)
    }
}

/// Reasons a block's native BLS transfers fail [`BlsBlockAuthorization::Verify`].
///
/// These describe the detached witness, not the canonical block: the same block may become
/// valid with a different aggregate, so block execution reports them as non-consensus
/// (`BlockExecutionError::other`) errors rather than invalid-block validation errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BlsBlockAuthorizationError {
    /// The block includes transfers but no aggregate signature was supplied.
    #[error("block includes {transfers} BLS transfers but no aggregate signature")]
    MissingAggregate {
        /// Number of included transfers.
        transfers: usize,
    },
    /// An aggregate signature was supplied for a block without transfers.
    #[error("BLS aggregate signature supplied for a block without transfers")]
    UnexpectedAggregate,
    /// The aggregate signature is not a canonical non-identity subgroup point.
    #[error("invalid BLS aggregate signature point")]
    InvalidAggregatePoint,
    /// The aggregate signature does not verify over the block's transfers.
    #[error("invalid BLS aggregate signature")]
    InvalidAggregate,
    /// A sender's registered key is not a valid non-identity subgroup point.
    #[error("BLS transfer sender {sender} has an invalid registered key")]
    InvalidPublicKey {
        /// The transfer sender.
        sender: Address,
    },
    /// A transfer was committed without its intent being collected during execution, so it
    /// cannot be verified.
    #[error("BLS transfer at position {index} was committed without an authorization intent")]
    UnattestedTransfer {
        /// Position of the transfer among the block's transfers.
        index: usize,
    },
}

impl BlsBlockAuthorization {
    /// Checks the block's committed transfers, in order, against this authorization.
    ///
    /// `transfers` has one entry per committed transfer: its intent, or `None` if the intent was
    /// not collected. [`Self::Derived`] accepts any block.
    pub fn verify(
        &self,
        transfers: &[Option<BlsTransferIntent>],
    ) -> Result<(), BlsBlockAuthorizationError> {
        let Self::Verify(aggregate) = self else {
            return Ok(());
        };
        let aggregate = match (aggregate, transfers.is_empty()) {
            (None, true) => return Ok(()),
            (Some(_), true) => return Err(BlsBlockAuthorizationError::UnexpectedAggregate),
            (None, false) => {
                return Err(BlsBlockAuthorizationError::MissingAggregate {
                    transfers: transfers.len(),
                });
            }
            (Some(aggregate), false) => aggregate,
        };
        let mut signers = Vec::with_capacity(transfers.len());
        for (index, intent) in transfers.iter().enumerate() {
            let intent = intent.ok_or(BlsBlockAuthorizationError::UnattestedTransfer { index })?;
            let key = BlsCrypto::public_key(&intent.public_key.0)
                .ok_or(BlsBlockAuthorizationError::InvalidPublicKey { sender: intent.sender })?;
            signers.push((key, intent.digest));
        }
        let aggregate = BlsCrypto::signature(&aggregate.0)
            .ok_or(BlsBlockAuthorizationError::InvalidAggregatePoint)?;
        if !BlsCrypto::aggregate_verify(&signers, aggregate) {
            return Err(BlsBlockAuthorizationError::InvalidAggregate);
        }
        Ok(())
    }
}
