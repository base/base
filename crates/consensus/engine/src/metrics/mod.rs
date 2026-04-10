//! Prometheus metrics collection for engine operations.
//!
//! Provides metric identifiers and labels for monitoring engine performance,
//! task execution, and block progression through safety levels.

base_metrics::define_metrics! {
    base_node
    #[describe("Blockchain head labels")]
    #[label(label)]
    block_labels: gauge,
    #[describe("Seconds behind wall clock for each blockchain head ref")]
    #[label(label)]
    block_refs_latency: gauge,
    #[describe("Engine tasks successfully executed")]
    #[label(
        name = "task",
        default = ["insert", "consolidate", "build", "finalize", "seal", "get-payload"]
    )]
    engine_task_count: counter,
    #[describe("Engine tasks failed")]
    #[label(
        name = "task",
        default = ["insert", "consolidate", "build", "finalize", "seal", "get-payload"]
    )]
    #[label(name = "severity", default = ["temporary", "critical", "reset", "flush"])]
    engine_task_failure: counter,
    #[describe("Engine method request duration")]
    #[label(method)]
    engine_method_request_duration: histogram,
    #[describe("Engine reset count")]
    engine_reset_count: counter,
    #[describe("Payloads dropped because unsafe head changed between build and seal")]
    sequencer_unsafe_head_changed_total: counter,
    #[describe("Number of tasks currently pending in the engine task queue")]
    engine_task_queue_depth: gauge,
}

impl Metrics {
    /// Unsafe block label.
    pub const UNSAFE_BLOCK_LABEL: &str = "unsafe";
    /// Safe block label.
    pub const SAFE_BLOCK_LABEL: &str = "safe";
    /// Finalized block label.
    pub const FINALIZED_BLOCK_LABEL: &str = "finalized";

    /// Insert task label.
    pub const INSERT_TASK_LABEL: &str = "insert";
    /// Consolidate task label.
    pub const CONSOLIDATE_TASK_LABEL: &str = "consolidate";
    /// Forkchoice task label.
    pub const FORKCHOICE_TASK_LABEL: &str = "forkchoice-update";
    /// Build task label.
    pub const BUILD_TASK_LABEL: &str = "build";
    /// Seal task label.
    pub const SEAL_TASK_LABEL: &str = "seal";
    /// Get-payload task label.
    pub const GET_PAYLOAD_TASK_LABEL: &str = "get-payload";
    /// Finalize task label.
    pub const FINALIZE_TASK_LABEL: &str = "finalize";

    /// Temporary severity label.
    pub const TEMPORARY_SEVERITY_LABEL: &str = "temporary";
    /// Critical severity label.
    pub const CRITICAL_SEVERITY_LABEL: &str = "critical";
    /// Reset severity label.
    pub const RESET_SEVERITY_LABEL: &str = "reset";
    /// Flush severity label.
    pub const FLUSH_SEVERITY_LABEL: &str = "flush";

    /// `engine_forkchoiceUpdatedV<N>` label
    pub const FORKCHOICE_UPDATE_METHOD: &str = "engine_forkchoiceUpdated";
    /// `engine_newPayloadV<N>` label.
    pub const NEW_PAYLOAD_METHOD: &str = "engine_newPayload";
    /// `engine_getPayloadV<N>` label.
    pub const GET_PAYLOAD_METHOD: &str = "engine_getPayload";
}
