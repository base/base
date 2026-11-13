use alloy_primitives::B256;
use alloy_rpc_types_engine::PayloadId;

/// An execution-layer notification that a sequencer payload has finished building and is frozen.
///
/// This in-process hint does not replace `getPayload`. Consumers must match both identifiers
/// against their active build and retain their normal seal deadline if a notification is lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadReady {
    /// The frozen payload's Engine API identifier.
    pub payload_id: PayloadId,
    /// The parent the payload was built on, for rejecting stale notifications after a reset.
    pub parent_hash: B256,
}
