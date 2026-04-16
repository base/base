//! Metrics for the node service

/// Container for metrics.
#[derive(Debug, Clone)]
pub struct Metrics;

impl Metrics {
    /// Identifier for the counter that tracks the number of times the L1 has reorganized.
    pub const L1_REORG_COUNT: &str = "base_node_l1_reorg_count";

    /// Identifier for the counter that tracks the L1 origin of the derivation pipeline.
    pub const DERIVATION_L1_ORIGIN: &str = "base_node_derivation_l1_origin";

    /// Identifier for the counter of critical derivation errors (strictly for alerting.)
    pub const DERIVATION_CRITICAL_ERROR: &str = "base_node_derivation_critical_errors";

    /// Identifier for the counter that tracks sequencer state flags.
    pub const SEQUENCER_STATE: &str = "base_node_sequencer_state";

    /// Gauge for the sequencer's attributes builder duration.
    pub const SEQUENCER_ATTRIBUTES_BUILDER_DURATION: &str =
        "base_node_sequencer_attributes_build_duration";

    /// Gauge for the sequencer's block building start task duration.
    pub const SEQUENCER_BLOCK_BUILDING_START_TASK_DURATION: &str =
        "base_node_sequencer_block_building_start_task_duration";

    /// Gauge for the sequencer's block building seal task duration.
    pub const SEQUENCER_BLOCK_BUILDING_SEAL_TASK_DURATION: &str =
        "base_node_sequencer_block_building_seal_task_duration";

    /// Gauge for the sequencer's conductor commitment duration.
    pub const SEQUENCER_CONDUCTOR_COMMITMENT_DURATION: &str =
        "base_node_sequencer_conductor_commitment_duration";

    /// Total number of transactions of sequenced by sequencer.
    pub const SEQUENCER_TOTAL_TRANSACTIONS_SEQUENCED: &str =
        "base_node_sequencer_total_transactions_sequenced";

    /// Counter for seal pipeline step retries, labeled by step ("conductor"|"gossip"|"insert").
    pub const SEQUENCER_SEAL_STEP_RETRIES_TOTAL: &str =
        "base_node_sequencer_seal_step_retries_total";
    /// Gauge for seal pipeline step duration, labeled by step.
    pub const SEQUENCER_SEAL_STEP_DURATION: &str = "base_node_sequencer_seal_step_duration";
    /// Counter for seal errors labeled by fatal ("true"|"false").
    pub const SEQUENCER_SEAL_ERROR_TOTAL: &str = "base_node_sequencer_seal_errors_total";
    /// Counter for sequencer start rejections labeled by reason.
    pub const SEQUENCER_START_REJECTED_TOTAL: &str = "base_node_sequencer_start_rejected_total";
    /// Counter for deferred `stop_sequencer` responses due to in-flight seal pipeline.
    pub const SEQUENCER_STOP_DEFERRED_TOTAL: &str = "base_node_sequencer_stop_deferred_total";
    /// Counter for blocks sequenced in recovery mode (always empty blocks).
    pub const SEQUENCER_RECOVERY_MODE_BLOCKS_TOTAL: &str =
        "base_node_sequencer_recovery_mode_blocks_total";
    /// Counter for empty blocks produced due to sequencer drift threshold.
    pub const SEQUENCER_DRIFT_EMPTY_BLOCKS_TOTAL: &str =
        "base_node_sequencer_drift_empty_blocks_total";
    /// Counter for pre-built payloads discarded because the unsafe head advanced past their
    /// parent before sealing (stale build detection).
    pub const SEQUENCER_STALE_BUILD_DISCARDED_TOTAL: &str =
        "base_node_sequencer_stale_build_discarded_total";

    /// Gauge for the configured verifier L1 confirmation depth.
    pub const L1_VERIFIER_CONFS_DEPTH: &str = "base_node_l1_verifier_confs_depth";
    /// Counter for the L1 block number forwarded to derivation after verifier confirmation delay.
    pub const L1_VERIFIER_DERIVATION_HEAD: &str = "base_node_l1_verifier_derivation_head";
    /// Counter for failed attempts to fetch a delayed L1 block for verifier confirmation.
    pub const L1_VERIFIER_DELAYED_FETCH_ERRORS: &str = "base_node_l1_verifier_delayed_fetch_errors";

    /// Initializes metrics for the node service.
    ///
    /// This does two things:
    /// * Describes various metrics.
    /// * Initializes metrics to 0 so they can be queried immediately.
    #[cfg(feature = "metrics")]
    pub fn init() {
        Self::describe();
        Self::zero();
    }

    /// Describes metrics used in [`base-consensus-node`][crate].
    #[cfg(feature = "metrics")]
    pub fn describe() {
        // L1 reorg count
        metrics::describe_counter!(Self::L1_REORG_COUNT, metrics::Unit::Count, "L1 reorg count");

        // Derivation L1 origin
        metrics::describe_counter!(Self::DERIVATION_L1_ORIGIN, "Derivation pipeline L1 origin");

        // Derivation critical error
        metrics::describe_counter!(
            Self::DERIVATION_CRITICAL_ERROR,
            "Critical errors in the derivation pipeline"
        );

        // Sequencer state
        metrics::describe_counter!(Self::SEQUENCER_STATE, "Tracks sequencer state flags");

        // Sequencer attributes builder duration
        metrics::describe_gauge!(
            Self::SEQUENCER_ATTRIBUTES_BUILDER_DURATION,
            "Duration of the sequencer attributes builder"
        );

        // Sequencer block building job duration
        metrics::describe_gauge!(
            Self::SEQUENCER_BLOCK_BUILDING_START_TASK_DURATION,
            "Duration of the sequencer block building start task"
        );

        // Sequencer block building job duration
        metrics::describe_gauge!(
            Self::SEQUENCER_BLOCK_BUILDING_SEAL_TASK_DURATION,
            "Duration of the sequencer block building seal task"
        );

        // Sequencer conductor commitment duration
        metrics::describe_gauge!(
            Self::SEQUENCER_CONDUCTOR_COMMITMENT_DURATION,
            "Duration of the sequencer conductor commitment"
        );

        // Sequencer total transactions sequenced
        metrics::describe_counter!(
            Self::SEQUENCER_TOTAL_TRANSACTIONS_SEQUENCED,
            metrics::Unit::Count,
            "Total count of sequenced transactions"
        );

        metrics::describe_counter!(
            Self::SEQUENCER_SEAL_STEP_RETRIES_TOTAL,
            "Sequencer seal step retries by step"
        );
        metrics::describe_gauge!(
            Self::SEQUENCER_SEAL_STEP_DURATION,
            metrics::Unit::Seconds,
            "Sequencer seal step duration by step"
        );
        metrics::describe_counter!(Self::SEQUENCER_SEAL_ERROR_TOTAL, "Seal errors by fatality");
        metrics::describe_counter!(
            Self::SEQUENCER_START_REJECTED_TOTAL,
            "Sequencer start rejections by reason"
        );
        metrics::describe_counter!(
            Self::SEQUENCER_STOP_DEFERRED_TOTAL,
            "Deferred stop_sequencer responses due to in-flight seal pipeline"
        );
        metrics::describe_counter!(
            Self::SEQUENCER_RECOVERY_MODE_BLOCKS_TOTAL,
            "Blocks sequenced in recovery mode"
        );
        metrics::describe_counter!(
            Self::SEQUENCER_DRIFT_EMPTY_BLOCKS_TOTAL,
            "Empty blocks produced due to sequencer drift threshold"
        );
        metrics::describe_counter!(
            Self::SEQUENCER_STALE_BUILD_DISCARDED_TOTAL,
            "Pre-built payloads discarded because the unsafe head advanced past their parent"
        );

        // Verifier L1 confirmation delay
        metrics::describe_gauge!(
            Self::L1_VERIFIER_CONFS_DEPTH,
            "Configured verifier L1 confirmation depth"
        );
        metrics::describe_counter!(
            Self::L1_VERIFIER_DERIVATION_HEAD,
            "L1 block number forwarded to derivation after verifier confirmation delay"
        );
        metrics::describe_counter!(
            Self::L1_VERIFIER_DELAYED_FETCH_ERRORS,
            "Failed attempts to fetch a delayed L1 block for verifier confirmation"
        );
    }

    /// Initializes metrics to `0` so they can be queried immediately by consumers of prometheus
    /// metrics.
    #[cfg(feature = "metrics")]
    pub fn zero() {
        // L1 reorg reset count
        base_metrics::set!(counter, Self::L1_REORG_COUNT, 0);

        // Derivation critical error
        base_metrics::set!(counter, Self::DERIVATION_CRITICAL_ERROR, 0);

        // Sequencer: reset total transactions sequenced
        base_metrics::set!(counter, Self::SEQUENCER_TOTAL_TRANSACTIONS_SEQUENCED, 0);

        base_metrics::set!(
            counter,
            Self::SEQUENCER_SEAL_STEP_RETRIES_TOTAL,
            "step",
            "conductor",
            0
        );
        base_metrics::set!(counter, Self::SEQUENCER_SEAL_STEP_RETRIES_TOTAL, "step", "gossip", 0);
        base_metrics::set!(counter, Self::SEQUENCER_SEAL_STEP_RETRIES_TOTAL, "step", "insert", 0);
        base_metrics::set!(counter, Self::SEQUENCER_SEAL_ERROR_TOTAL, "fatal", "true", 0);
        base_metrics::set!(counter, Self::SEQUENCER_SEAL_ERROR_TOTAL, "fatal", "false", 0);
        base_metrics::set!(
            counter,
            Self::SEQUENCER_START_REJECTED_TOTAL,
            "reason",
            "not_leader",
            0
        );
        base_metrics::set!(
            counter,
            Self::SEQUENCER_START_REJECTED_TOTAL,
            "reason",
            "leadership_check_failed",
            0
        );
        base_metrics::set!(counter, Self::SEQUENCER_STOP_DEFERRED_TOTAL, 0);
        base_metrics::set!(counter, Self::SEQUENCER_RECOVERY_MODE_BLOCKS_TOTAL, 0);
        base_metrics::set!(counter, Self::SEQUENCER_DRIFT_EMPTY_BLOCKS_TOTAL, 0);
        base_metrics::set!(counter, Self::SEQUENCER_STALE_BUILD_DISCARDED_TOTAL, 0);
        base_metrics::set!(counter, Self::L1_VERIFIER_DELAYED_FETCH_ERRORS, 0);
    }
}
