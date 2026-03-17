//! Internal driver event type produced by the `tokio::select!` I/O phase.

use base_alloy_consensus::OpBlock;
use base_batcher_encoder::SubmissionId;
use base_protocol::L2BlockInfo;

use crate::TxOutcome;

/// Events the driver can receive from external sources during the I/O phase.
#[derive(Debug)]
pub enum DriverEvent {
    /// Cancellation token fired, or L2 source signalled exhausted.
    Shutdown,
    /// New L2 unsafe block from the source.
    Block(Box<OpBlock>),
    /// Source requested a force-flush of the current channel.
    Flush,
    /// L2 reorganisation; new safe head provided.
    Reorg(L2BlockInfo),
    /// An in-flight L1 transaction settled.
    Receipt(SubmissionId, TxOutcome),
    /// L1 chain head advanced.
    L1Head(u64),
    /// Safe L2 head advanced (from watch channel).
    SafeHead(u64),
    /// L1 head source permanently closed (Exhausted or Closed error).
    L1SourceClosed,
}
