//! Backend abstraction over the sequencer's engine client.
//!
//! [`L2Sequencer`](crate::L2Sequencer) is generic over this trait so it can drive either the
//! in-memory [`ActionEngineClient`](crate::ActionEngineClient) (the default, "Light" backend) or the
//! production-builder-backed [`BuilderBackedEngineClient`](crate::BuilderBackedEngineClient) through
//! the same production `SequencerActor` seam.

use async_trait::async_trait;
use base_common_consensus::BaseTxEnvelope;
use base_consensus_node::EngineClientResult;

use crate::SharedBlockHashRegistry;

/// A sequencer engine backend: a [`SequencerEngineClient`](base_consensus_node::SequencerEngineClient)
/// that also exposes the shared block-hash registry used for sequencer↔verifier state-root
/// cross-checks, plus how harness-supplied transactions reach a produced block.
#[async_trait]
pub trait SequencerEngineBackend: base_consensus_node::SequencerEngineClient + 'static {
    /// Return the shared block-hash registry this backend writes produced blocks into.
    fn block_hash_registry(&self) -> SharedBlockHashRegistry;

    /// Whether this backend selects transactions from a real mempool.
    ///
    /// When `false` (the default, in-memory backend), harness-supplied transactions are force
    /// included via the payload attributes with `no_tx_pool = true`. When `true`, they are injected
    /// into the backend's real pool via [`inject_pool_transactions`](Self::inject_pool_transactions)
    /// and selected by the production builder with `no_tx_pool = false`.
    fn uses_transaction_pool(&self) -> bool {
        false
    }

    /// Inject harness-supplied transactions into the backend's mempool.
    ///
    /// No-op for force-attribute backends (which never consult a pool).
    ///
    /// Implementations backed by an external pool may not insert a batch atomically. If this
    /// returns an error, a prefix of `txs` may remain queued and surface in a later block.
    async fn inject_pool_transactions(&self, _txs: Vec<BaseTxEnvelope>) -> EngineClientResult<()> {
        Ok(())
    }
}
