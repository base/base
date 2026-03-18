//! Pending proof state machine and collection.
//!
//! [`PendingProofs`] tracks in-flight ZK proof sessions keyed by dispute-game
//! address. Each entry moves through a [`ProofPhase`] lifecycle:
//! `AwaitingProof` → `ReadyToSubmit` (on success) or `NeedsRetry` (on failure).

use std::collections::HashMap;

use alloy_primitives::{Address, B256, Bytes};
use base_zk_client::{
    GetProofRequest, ProofJobStatus, ProveBlockRequest, ReceiptType, ZkProofProvider,
};
use tracing::warn;

/// Proof type byte for ZK proofs (matches `AggregateVerifier.nullify` discriminator: `0` = TEE, `1` = ZK).
const ZK_PROOF_TYPE_BYTE: u8 = 0x01;

/// Phase of a pending proof: awaiting the ZK service, ready for on-chain
/// submission, or waiting for a retry after failure.
#[derive(Debug, Clone)]
pub enum ProofPhase {
    /// Waiting for the ZK proof service to complete.
    AwaitingProof {
        /// Session ID returned by the ZK proof service.
        session_id: String,
    },
    /// Proof obtained — receipt bytes are ready for nullification submission.
    ReadyToSubmit {
        /// Type-prefixed proof receipt bytes.
        proof_bytes: Bytes,
    },
    /// Proof job failed — the driver should re-initiate `prove_block`.
    NeedsRetry,
}

/// State for an in-flight proof session.
#[derive(Debug, Clone)]
pub struct PendingProof {
    /// Current phase of this proof lifecycle.
    pub phase: ProofPhase,
    /// The index of the invalid intermediate root.
    pub invalid_index: u64,
    /// The expected correct root at that index.
    pub expected_root: B256,
    /// Original request parameters, stored so the driver can re-initiate on failure.
    pub prove_request: ProveBlockRequest,
    /// Number of times this proof has been retried after failure.
    pub retry_count: u32,
}

impl PendingProof {
    /// Creates a new `PendingProof` in the `AwaitingProof` phase.
    pub const fn awaiting(
        session_id: String,
        invalid_index: u64,
        expected_root: B256,
        prove_request: ProveBlockRequest,
    ) -> Self {
        Self {
            phase: ProofPhase::AwaitingProof { session_id },
            invalid_index,
            expected_root,
            prove_request,
            retry_count: 0,
        }
    }

    /// Creates a new `PendingProof` in the `ReadyToSubmit` phase.
    pub const fn ready(
        proof_bytes: Bytes,
        invalid_index: u64,
        expected_root: B256,
        prove_request: ProveBlockRequest,
    ) -> Self {
        Self {
            phase: ProofPhase::ReadyToSubmit { proof_bytes },
            invalid_index,
            expected_root,
            prove_request,
            retry_count: 0,
        }
    }

    /// Returns the session ID if the proof is in the `AwaitingProof` phase.
    pub fn session_id(&self) -> Option<&str> {
        match &self.phase {
            ProofPhase::AwaitingProof { session_id } => Some(session_id),
            _ => None,
        }
    }

    /// Returns the proof bytes if the proof is in the `ReadyToSubmit` phase.
    pub const fn proof_bytes(&self) -> Option<&Bytes> {
        match &self.phase {
            ProofPhase::ReadyToSubmit { proof_bytes } => Some(proof_bytes),
            _ => None,
        }
    }

    /// Returns `true` if the proof is in the `ReadyToSubmit` phase.
    pub const fn is_ready(&self) -> bool {
        matches!(self.phase, ProofPhase::ReadyToSubmit { .. })
    }
}

/// Collection of in-flight proof sessions keyed by dispute-game address.
#[derive(Debug, Clone, Default)]
pub struct PendingProofs(HashMap<Address, PendingProof>);

impl PendingProofs {
    /// Creates an empty collection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a pending proof for the given game address.
    pub fn insert(&mut self, game: Address, proof: PendingProof) {
        self.0.insert(game, proof);
    }

    /// Removes the pending proof for the given game address.
    pub fn remove(&mut self, game: &Address) {
        self.0.remove(game);
    }

    /// Returns a reference to the pending proof for the given game address.
    pub fn get(&self, game: &Address) -> Option<&PendingProof> {
        self.0.get(game)
    }

    /// Returns a mutable reference to the pending proof for the given game address.
    pub fn get_mut(&mut self, game: &Address) -> Option<&mut PendingProof> {
        self.0.get_mut(game)
    }

    /// Returns `true` if there is a pending proof for the given game address.
    pub fn contains_key(&self, game: &Address) -> bool {
        self.0.contains_key(game)
    }

    /// Returns the game addresses with pending proofs.
    pub fn addresses(&self) -> Vec<Address> {
        self.0.keys().copied().collect()
    }

    /// Returns the number of pending proofs.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if there are no pending proofs.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Polls the ZK service for an in-flight proof and advances the entry.
    ///
    /// - **`AwaitingProof`** — sends a `GetProofRequest` to the ZK service
    ///   and transitions the entry based on the response status.
    /// - **`ReadyToSubmit`** — returns [`ProofUpdate::Ready`] immediately.
    /// - **`NeedsRetry`** — returns [`ProofUpdate::NeedsRetry`] immediately
    ///   so the driver can re-initiate `prove_block`.
    ///
    /// Returns `Ok(None)` when `game` has no entry in the collection.
    pub async fn poll<P: ZkProofProvider>(
        &mut self,
        game: Address,
        zk_prover: &P,
    ) -> eyre::Result<Option<ProofUpdate>> {
        let pending = match self.0.get(&game) {
            Some(p) => p,
            None => return Ok(None),
        };

        let session_id = match &pending.phase {
            ProofPhase::AwaitingProof { session_id } => session_id.clone(),
            ProofPhase::ReadyToSubmit { proof_bytes } => {
                return Ok(Some(ProofUpdate::Ready(proof_bytes.clone())));
            }
            ProofPhase::NeedsRetry => {
                return Ok(Some(ProofUpdate::NeedsRetry));
            }
        };

        let request = GetProofRequest { session_id, receipt_type: Some(ReceiptType::Snark as i32) };

        let response = zk_prover.get_proof(request).await?;
        let status = ProofJobStatus::try_from(response.status).unwrap_or_else(|_| {
            warn!(raw_status = response.status, game = %game, "unrecognized proof job status");
            ProofJobStatus::Unspecified
        });

        // Re-borrow after the await point.
        let pending = match self.0.get_mut(&game) {
            Some(p) => p,
            None => return Ok(None),
        };

        let update = match status {
            ProofJobStatus::Succeeded => {
                let mut raw = Vec::with_capacity(1 + response.receipt.len());
                raw.push(ZK_PROOF_TYPE_BYTE);
                raw.extend_from_slice(&response.receipt);
                let proof_bytes = Bytes::from(raw);

                pending.phase = ProofPhase::ReadyToSubmit { proof_bytes: proof_bytes.clone() };

                ProofUpdate::Ready(proof_bytes)
            }
            ProofJobStatus::Failed => {
                pending.retry_count += 1;
                pending.phase = ProofPhase::NeedsRetry;
                ProofUpdate::NeedsRetry
            }
            _ => ProofUpdate::Pending,
        };

        Ok(Some(update))
    }
}

/// Result of advancing a pending proof via [`PendingProofs::poll`].
#[derive(Debug, Clone)]
pub enum ProofUpdate {
    /// The proof succeeded — type-prefixed bytes are ready for submission.
    Ready(Bytes),
    /// The proof job failed — the entry is retained for retry.
    NeedsRetry,
    /// The proof is still in progress.
    Pending,
}
