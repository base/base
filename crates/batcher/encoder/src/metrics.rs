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
    #[describe("Number of input bytes to a channel")]
    #[label(name = "stage", default = ["added", "closed"])]
    input_bytes: gauge,
    #[describe("Number of compressed output bytes from a channel")]
    output_bytes: gauge,
    #[describe("Total number of input bytes to channels")]
    input_bytes_total: counter,
    #[describe("Total number of compressed output bytes from channels")]
    output_bytes_total: counter,
    #[describe("Total number of frames in the closed channel")]
    channel_num_frames: gauge,
    #[describe("Batcher signer account balance in ether")]
    #[no_zero]
    balance: gauge,
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

    // ======================================================================
    // TEMPORARY — shadow base-batcher rollout
    //
    // Feeding the shadow batcher Datadog dashboard on Zeronet. Deliberately
    // additive: some of these duplicate a series above instead of reshaping
    // it, so nothing already deployed breaks. Revisit once the rollout ends,
    // either folding them into the metrics above with their owners or dropping
    // them.
    // ======================================================================
    #[describe("Total number of encoding channels closed, by fine-grained cause")]
    #[label(
        name = "cause",
        default = ["soft_target", "protocol_limit", "timeout", "flush", "discard"]
    )]
    channel_close_cause_total: counter,
    #[describe("Total number of times buffered pipeline state was dropped and rebuilt")]
    #[label(
        name = "reason",
        default = [
            "source_reorg",
            "ingest_reorg",
            "safe_head_reorg",
            "safe_head_mismatch",
            "stalled_channel",
            "admin_pause",
        ]
    )]
    pipeline_reset_total: counter,
}

impl BatcherMetrics {
    /// Channel closed because the compressed frame data reached the target size.
    pub const REASON_SIZE_FULL: &'static str = "size_full";

    /// Channel closed because it reached `max_channel_duration` L1 blocks.
    pub const REASON_TIMEOUT: &'static str = "timeout";

    /// Channel closed by an explicit force-flush signal.
    pub const REASON_FORCE: &'static str = "force";

    /// Channel discarded because its first block exceeded channel limits.
    pub const REASON_DISCARD: &'static str = "discard";

    /// Channel input bytes after blocks have been added.
    pub const STAGE_ADDED: &'static str = "added";

    /// Channel input bytes after the channel has been closed.
    pub const STAGE_CLOSED: &'static str = "closed";

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

    // ======================================================================
    // TEMPORARY — shadow base-batcher rollout: label values for the metrics
    // added above.
    // ======================================================================

    /// Channel closed because its soft compressed-size target was reached.
    ///
    /// Reported as `size_full` by `channel_closed_total`, which cannot distinguish
    /// this from a hard protocol limit.
    pub const CAUSE_SOFT_TARGET: &'static str = "soft_target";

    /// Channel closed before a batch that would exceed a hard protocol limit.
    ///
    /// Also reported as `size_full` by `channel_closed_total`.
    pub const CAUSE_PROTOCOL_LIMIT: &'static str = "protocol_limit";

    /// Channel closed because it reached `max_channel_duration` L1 blocks.
    pub const CAUSE_TIMEOUT: &'static str = "timeout";

    /// Channel closed by an explicit flush signal.
    pub const CAUSE_FLUSH: &'static str = "flush";

    /// Channel discarded because its first block exceeded channel limits.
    pub const CAUSE_DISCARD: &'static str = "discard";

    /// The block source signalled an L2 reorg.
    pub const RESET_SOURCE_REORG: &'static str = "source_reorg";

    /// A parent-hash mismatch surfaced while adding a block to the pipeline.
    pub const RESET_INGEST_REORG: &'static str = "ingest_reorg";

    /// The derivation status reported a safe head that moved back or changed hash.
    pub const RESET_SAFE_HEAD_REORG: &'static str = "safe_head_reorg";

    /// The derived safe head does not match the buffered chain.
    pub const RESET_SAFE_HEAD_MISMATCH: &'static str = "safe_head_mismatch";

    /// The rollup node passed a fully confirmed channel without deriving it.
    pub const RESET_STALLED_CHANNEL: &'static str = "stalled_channel";

    /// The batcher was paused through the admin API.
    pub const RESET_ADMIN_PAUSE: &'static str = "admin_pause";
}
