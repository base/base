//! Batcher metric definitions and label values.

base_metrics::define_metrics! {
    batcher,
    struct = BatcherMetrics,
    #[describe("Total number of encoding channels opened")]
    channel_opened_total: counter,
    #[describe("Total number of encoding channels closed")]
    #[label(reason)]
    channel_closed_total: counter,
    #[describe("Total number of channels for which every frame was confirmed on L1")]
    channel_fully_submitted_total: counter,
    #[describe("Total number of L1 batch submissions")]
    #[label(outcome)]
    submission_total: counter,
    #[describe("Total bytes of frame payload submitted to the DA layer")]
    #[label(da_type)]
    da_bytes_submitted_total: counter,
    #[describe("Total bytes of frame payload packed into EIP-4844 blobs")]
    blob_used_bytes_total: counter,
    #[describe("Number of frames currently waiting for L1 submission")]
    pending_frames: gauge,
    #[describe("Number of L2 blocks buffered in the encoder input queue")]
    pending_blocks: gauge,
    #[describe("Number of L1 transactions currently in-flight")]
    in_flight_submissions: gauge,
    #[describe("Compression ratio for each closed channel")]
    channel_compression_ratio: histogram,
    #[describe("Channel lifetime in L1 blocks")]
    channel_duration_blocks: histogram,
    #[describe("Number of L2 blocks included in each closed channel")]
    l2_blocks_per_channel: histogram,
}

impl BatcherMetrics {
    /// Channel closed because the compressed frame data reached the target size.
    pub const REASON_SIZE_FULL: &'static str = "size_full";

    /// Channel closed because it reached `max_channel_duration` L1 blocks.
    pub const REASON_TIMEOUT: &'static str = "timeout";

    /// Channel closed by an explicit force-flush signal.
    pub const REASON_FORCE: &'static str = "force";

    /// Channel discarded without producing frames because the span batch exceeded limits.
    pub const REASON_DISCARD: &'static str = "discard";

    /// Submission accepted and handed to the tx manager.
    pub const OUTCOME_SUBMITTED: &'static str = "submitted";

    /// Submission confirmed on L1.
    pub const OUTCOME_CONFIRMED: &'static str = "confirmed";

    /// Submission failed (tx reverted or timed out) and was requeued.
    pub const OUTCOME_FAILED: &'static str = "failed";

    /// Submission requeued due to txpool blockage.
    pub const OUTCOME_REQUEUED: &'static str = "requeued";

    /// Blob DA: frames encoded into EIP-4844 blobs.
    pub const DA_TYPE_BLOB: &'static str = "blob";

    /// Calldata DA: frames encoded as L1 transaction calldata.
    pub const DA_TYPE_CALLDATA: &'static str = "calldata";
}
