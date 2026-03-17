//! Batcher metric name constants and label values.
//!
//! All metrics use the `batcher_*` naming prefix. Constants here define the
//! canonical names and label values used across the encoder and driver so that
//! all instrumentation sites agree on the same strings at compile time. Living
//! in the encoder crate (the root of the batcher dependency graph) avoids a
//! circular dependency: both `base-batcher-encoder` and `base-batcher-core`
//! can import from here without introducing a cycle. Core re-exports
//! [`BatcherMetrics`] so downstream consumers have a single import path.
//!
//! Because the [`metrics`] crate dispatches by name at runtime, no registration
//! step is needed. Any recorder installed before the first use of a
//! `counter!`/`gauge!`/`histogram!` macro will automatically capture the metric.
//!
//! # Metric inventory
//!
//! | Kind      | Name                              | Labels                         |
//! |-----------|-----------------------------------|--------------------------------|
//! | Counter   | `batcher_channel_opened_total`    | —                              |
//! | Counter   | `batcher_channel_closed_total`    | `reason`                       |
//! | Counter   | `batcher_submission_total`        | `outcome`                      |
//! | Counter   | `batcher_da_bytes_submitted_total`| `da_type`                      |
//! | Gauge     | `batcher_pending_frames`          | —                              |
//! | Gauge     | `batcher_pending_blocks`          | —                              |
//! | Gauge     | `batcher_in_flight_submissions`   | —                              |
//! | Histogram | `batcher_channel_compression_ratio` | —                            |
//! | Histogram | `batcher_channel_duration_blocks` | —                              |

/// Metric name constants and label values for all batcher metrics.
///
/// Use the constants as arguments to the [`metrics`] crate macros:
///
/// ```ignore
/// use metrics::{counter, gauge, histogram};
/// use base_batcher_encoder::BatcherMetrics;
///
/// counter!(BatcherMetrics::CHANNEL_OPENED_TOTAL).increment(1);
/// gauge!(BatcherMetrics::PENDING_BLOCKS).increment(1.0);
/// histogram!(BatcherMetrics::CHANNEL_DURATION_BLOCKS).record(4.0);
/// ```
#[derive(Debug)]
pub struct BatcherMetrics;

impl BatcherMetrics {
    // ── Counters ──────────────────────────────────────────────────────────────

    /// Total number of encoding channels opened.
    pub const CHANNEL_OPENED_TOTAL: &'static str = "batcher_channel_opened_total";

    /// Total number of encoding channels closed, labelled by close `reason`.
    ///
    /// See [`REASON_SIZE_FULL`], [`REASON_TIMEOUT`], [`REASON_FORCE`] for valid
    /// label values.
    ///
    /// [`REASON_SIZE_FULL`]: BatcherMetrics::REASON_SIZE_FULL
    /// [`REASON_TIMEOUT`]: BatcherMetrics::REASON_TIMEOUT
    /// [`REASON_FORCE`]: BatcherMetrics::REASON_FORCE
    pub const CHANNEL_CLOSED_TOTAL: &'static str = "batcher_channel_closed_total";

    /// Total number of L1 batch submissions, labelled by `outcome`.
    ///
    /// See [`OUTCOME_SUBMITTED`], [`OUTCOME_CONFIRMED`], [`OUTCOME_FAILED`],
    /// [`OUTCOME_REQUEUED`] for valid label values.
    ///
    /// [`OUTCOME_SUBMITTED`]: BatcherMetrics::OUTCOME_SUBMITTED
    /// [`OUTCOME_CONFIRMED`]: BatcherMetrics::OUTCOME_CONFIRMED
    /// [`OUTCOME_FAILED`]: BatcherMetrics::OUTCOME_FAILED
    /// [`OUTCOME_REQUEUED`]: BatcherMetrics::OUTCOME_REQUEUED
    pub const SUBMISSION_TOTAL: &'static str = "batcher_submission_total";

    /// Total bytes of frame payload submitted to the DA layer, labelled by
    /// `da_type`. Counts raw frame data bytes, not blob-padded sizes.
    ///
    /// See [`DA_TYPE_BLOB`], [`DA_TYPE_CALLDATA`] for valid label values.
    ///
    /// [`DA_TYPE_BLOB`]: BatcherMetrics::DA_TYPE_BLOB
    /// [`DA_TYPE_CALLDATA`]: BatcherMetrics::DA_TYPE_CALLDATA
    pub const DA_BYTES_SUBMITTED_TOTAL: &'static str = "batcher_da_bytes_submitted_total";

    // ── Gauges ────────────────────────────────────────────────────────────────

    /// Number of frames currently waiting in ready channels for L1 submission.
    ///
    /// Incremented when a channel is closed (all its frames become pending).
    /// Decremented when frames are handed to the tx manager via
    /// `next_submission`, and re-incremented when a submission is requeued.
    pub const PENDING_FRAMES: &'static str = "batcher_pending_frames";

    /// Number of L2 blocks buffered in the encoder input queue awaiting
    /// encoding. Incremented on `add_block`, decremented when blocks are
    /// pruned after confirmation or safe-head advancement.
    pub const PENDING_BLOCKS: &'static str = "batcher_pending_blocks";

    /// Number of L1 transactions currently in-flight (sent, awaiting receipt).
    /// Incremented on `submit_pending`, decremented when a receipt resolves.
    pub const IN_FLIGHT_SUBMISSIONS: &'static str = "batcher_in_flight_submissions";

    // ── Histograms ────────────────────────────────────────────────────────────

    /// Compression ratio for each closed channel, computed as
    /// `compressed_bytes / rlp_input_bytes`. Values below 1.0 indicate
    /// compression savings; the Brotli-10 shadow compressor typically achieves
    /// ~0.6 on batch data.
    ///
    /// Only recorded when the channel had non-zero input bytes.
    pub const CHANNEL_COMPRESSION_RATIO: &'static str = "batcher_channel_compression_ratio";

    /// Lifetime of a closed channel measured in L1 blocks elapsed since the
    /// channel was opened. Gives visibility into whether channels are closing
    /// on size or on timeout.
    pub const CHANNEL_DURATION_BLOCKS: &'static str = "batcher_channel_duration_blocks";

    // ── `reason` label values for CHANNEL_CLOSED_TOTAL ───────────────────────

    /// Channel closed because the compressed frame data reached the target size.
    pub const REASON_SIZE_FULL: &'static str = "size_full";

    /// Channel closed because it reached `max_channel_duration` L1 blocks.
    pub const REASON_TIMEOUT: &'static str = "timeout";

    /// Channel closed by an explicit force-flush signal (e.g. operator flush or
    /// graceful shutdown).
    pub const REASON_FORCE: &'static str = "force";

    /// Channel discarded without producing frames because the span batch it was
    /// opened for exceeded `MAX_RLP_BYTES_PER_CHANNEL` and `add_batch` rejected
    /// it. Keeps the opened/closed counters balanced.
    pub const REASON_DISCARD: &'static str = "discard";

    // ── `outcome` label values for SUBMISSION_TOTAL ──────────────────────────

    /// Submission accepted and handed to the tx manager.
    pub const OUTCOME_SUBMITTED: &'static str = "submitted";

    /// Submission confirmed on L1.
    pub const OUTCOME_CONFIRMED: &'static str = "confirmed";

    /// Submission failed (tx reverted or timed out) and was requeued.
    pub const OUTCOME_FAILED: &'static str = "failed";

    /// Submission requeued due to txpool blockage (nonce slot reserved).
    pub const OUTCOME_REQUEUED: &'static str = "requeued";

    // ── `da_type` label values for DA_BYTES_SUBMITTED_TOTAL ─────────────────

    /// Blob DA: frames encoded into EIP-4844 blobs.
    pub const DA_TYPE_BLOB: &'static str = "blob";

    /// Calldata DA: frames encoded as L1 transaction calldata.
    pub const DA_TYPE_CALLDATA: &'static str = "calldata";
}
