//! Internal driver event type produced by the `tokio::select!` I/O phase.

use base_batcher_encoder::SubmissionId;
use base_common_consensus::BaseBlock;
use tokio::sync::oneshot;

use crate::{DerivationStatus, TxOutcome};

/// Events the driver can receive from external sources during the I/O phase.
#[derive(Debug)]
pub enum DriverEvent {
    /// Cancellation token fired, or L2 source signalled exhausted.
    Shutdown,
    /// New L2 unsafe block from the source.
    Block(Box<BaseBlock>),
    /// Source (or admin) requested a force-flush of the current channel.
    ///
    /// If the ack is set, it fires once every frame resulting from this flush has been
    /// encoded and handed to the tx manager.
    Flush(Option<oneshot::Sender<()>>),
    /// L2 reorganisation detected.
    Reorg,
    /// An in-flight L1 transaction settled, carrying one or more submissions.
    Receipt(Vec<SubmissionId>, TxOutcome),
    /// L1 chain head advanced.
    L1Head(u64),
    /// Derivation progress changed.
    DerivationStatus(DerivationStatus),
    /// L1 head source permanently closed (Exhausted or Closed error).
    L1SourceClosed,
}
