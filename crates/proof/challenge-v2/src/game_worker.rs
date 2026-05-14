//! Shared dependencies and config for game workers.

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use base_zk_client::ZkProofProvider;
use derive_more::Debug;

use crate::TeeProofProvider;

/// Read-only handles and config shared across every game worker task.
///
/// Cloned via `Arc<WorkerDeps>` when spawning workers, so spawning is
/// cheap and all tasks observe the same configured services.
#[derive(Debug)]
pub struct WorkerDeps {
    /// ZK proving service. Generates SNARK proofs over disputed L2
    /// block ranges; used by every dispute path that needs a
    /// cryptographic on-chain proof.
    #[debug(skip)]
    pub zk_prover: Arc<dyn ZkProofProvider>,
    /// TEE proving service. Signs attestations over disputed L2
    /// block ranges; used as a fast path when the dispute can be
    /// settled by a TEE signature, with ZK as the fallback.
    #[debug(skip)]
    pub tee_prover: Arc<dyn TeeProofProvider>,
    /// Static worker config.
    pub config: WorkerConfig,
}

impl WorkerDeps {
    /// Bundles the proving services and config for sharing across workers.
    pub fn new(
        zk_prover: Arc<dyn ZkProofProvider>,
        tee_prover: Arc<dyn TeeProofProvider>,
        config: WorkerConfig,
    ) -> Self {
        Self { zk_prover, tee_prover, config }
    }
}

/// Per-worker configuration. `Copy` so it flows through async boundaries
/// without atomics or clones.
#[derive(Debug, Clone, Copy)]
pub struct WorkerConfig {
    /// Address that will sign and submit dispute transactions on L1.
    /// Forwarded to the ZK service so the SNARK journal commits to
    /// the same `msg.sender` the contract will see.
    pub sender_address: Address,
    /// Number of additional ZK proof attempts after the first one.
    /// Total attempts equals `max_proof_retries + 1`.
    pub max_proof_retries: u32,
    /// Sleep between successive ZK proof status polls while waiting
    /// for a job to reach a terminal state.
    pub proof_poll_interval: Duration,
    /// Per-attempt deadline for ZK proving. When exceeded the attempt
    /// is abandoned and a retry, if any remains, is initiated.
    pub max_proof_duration: Duration,
}
