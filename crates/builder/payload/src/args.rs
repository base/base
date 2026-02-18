/// Build arguments for payload construction.
use std::sync::Arc;

use parking_lot::Mutex;
use reth_basic_payload_builder::PayloadConfig;
use reth_payload_primitives::BuiltPayload;
use reth_primitives_traits::HeaderTy;
use reth_revm::cached::CachedReads;
use tokio_util::sync::CancellationToken;

use crate::BlockCell;

/// Build arguments
#[derive(Debug)]
pub struct BuildArguments<Attributes, Payload: BuiltPayload> {
    /// Previously cached disk reads
    pub cached_reads: CachedReads,
    /// How to configure the payload.
    pub config: PayloadConfig<Attributes, HeaderTy<Payload::Primitives>>,
    /// A marker that can be used to cancel the job.
    pub cancel: CancellationToken,
    /// Mutex to synchronize cancellation with payload publishing.
    pub publish_guard: Arc<Mutex<()>>,
    /// Cell to store the finalized payload with state root.
    pub finalized_cell: BlockCell<Payload>,
    /// Whether to compute state root only on finalization (when `get_payload` is called).
    pub compute_state_root_on_finalize: bool,
}
