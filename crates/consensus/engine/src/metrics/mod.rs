//! Prometheus metrics collection for engine operations.
//!
//! Provides metric identifiers and labels for monitoring engine performance,
//! task execution, and block progression through safety levels.

base_metrics::define_metrics! {
    base_node
    #[describe("Blockchain head labels")]
    #[label(label)]
    block_labels: gauge,
    #[describe("Engine tasks successfully executed")]
    #[label(task)]
    engine_task_count: counter,
    #[describe("Engine tasks failed")]
    #[label(task)]
    #[label(severity)]
    engine_task_failure: counter,
    #[describe("Engine method request duration")]
    #[label(method)]
    engine_method_request_duration: histogram,
    #[describe("Engine reset count")]
    engine_reset_count: counter,
    #[describe("Payloads dropped because unsafe head changed between build and seal")]
    sequencer_unsafe_head_changed_total: counter,
}

impl Metrics {
    /// Unsafe block label.
    pub const UNSAFE_BLOCK_LABEL: &str = "unsafe";
    /// Cross-unsafe block label.
    pub const CROSS_UNSAFE_BLOCK_LABEL: &str = "cross-unsafe";
    /// Local-safe block label.
    pub const LOCAL_SAFE_BLOCK_LABEL: &str = "local-safe";
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

    /// Initializes metrics for the engine.
    ///
    /// This does two things:
    /// * Describes various metrics.
    /// * Initializes metrics to 0 so they can be queried immediately.
    #[cfg(feature = "metrics")]
    pub fn init() {
        Self::describe();
        Self::zero();
    }

    /// Initializes metrics to `0` so they can be queried immediately by consumers of prometheus
    /// metrics.
    pub fn zero() {
        Self::engine_task_count(Self::INSERT_TASK_LABEL).absolute(0);
        Self::engine_task_count(Self::CONSOLIDATE_TASK_LABEL).absolute(0);
        Self::engine_task_count(Self::BUILD_TASK_LABEL).absolute(0);
        Self::engine_task_count(Self::FINALIZE_TASK_LABEL).absolute(0);
        Self::engine_task_count(Self::SEAL_TASK_LABEL).absolute(0);
        Self::engine_task_count(Self::GET_PAYLOAD_TASK_LABEL).absolute(0);

        for task in [
            Self::INSERT_TASK_LABEL,
            Self::CONSOLIDATE_TASK_LABEL,
            Self::BUILD_TASK_LABEL,
            Self::FINALIZE_TASK_LABEL,
            Self::SEAL_TASK_LABEL,
            Self::GET_PAYLOAD_TASK_LABEL,
        ] {
            for severity in [
                Self::TEMPORARY_SEVERITY_LABEL,
                Self::CRITICAL_SEVERITY_LABEL,
                Self::RESET_SEVERITY_LABEL,
                Self::FLUSH_SEVERITY_LABEL,
            ] {
                Self::engine_task_failure(task, severity).absolute(0);
            }
        }

        Self::engine_reset_count().absolute(0);
        Self::sequencer_unsafe_head_changed_total().absolute(0);
    }
}
