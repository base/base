//! Canonical block processors shipped with Base.

use base_execution_engine_observers::ExExContext;
use base_execution_state_indexer::ShadowIndexerExEx;
use futures::future::BoxFuture;

use crate::ProofHistory;

/// The canonical processors built into the Base node.
#[derive(Debug)]
pub enum BaseExecutionService {
    /// Persistent historical proofs.
    Proofs(ProofHistory),
    /// Shadow block indexing.
    Shadow(ShadowIndexerExEx),
}

impl BaseExecutionService {
    /// Stable WAL identity for this processor.
    pub const fn id(&self) -> &'static str {
        match self {
            Self::Proofs(_) => "proofs-history",
            Self::Shadow(_) => "shadow-indexer",
        }
    }

    /// Runs the processor against canonical notifications.
    pub fn run(self, ctx: ExExContext) -> BoxFuture<'static, eyre::Result<()>> {
        match self {
            Self::Proofs(proofs) => proofs.run(ctx),
            Self::Shadow(shadow) => Box::pin(shadow.run(ctx)),
        }
    }
}
