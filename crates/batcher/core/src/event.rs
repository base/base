//! Internal driver event type produced by the `tokio::select!` I/O phase.

use base_batcher_encoder::SubmissionId;
use base_common_consensus::BaseBlock;
use tokio::sync::oneshot;

use crate::{AdminResult, DerivationStatus, TxOutcome};

/// Events the driver can receive from external sources during the I/O phase.
#[derive(Debug)]
pub enum DriverEvent {
    /// Cancellation token fired.
    Shutdown,
    /// New L2 unsafe block from the source.
    Block(Box<BaseBlock>),
    /// Admin requested a flush of the current channel; answered once the pipeline is flushed.
    Flush(oneshot::Sender<AdminResult<()>>),
    /// L2 reorganisation detected.
    Reorg,
    /// An in-flight L1 transaction settled, carrying one or more submissions.
    Receipt(Vec<SubmissionId>, TxOutcome),
    /// L1 chain head advanced.
    L1Head(u64),
    /// Derivation progress changed.
    DerivationStatus(DerivationStatus),
    /// L1 head source permanently closed.
    L1SourceClosed,
}
