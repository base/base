//! Metrics for the node service

base_metrics::define_metrics! {
    base_node
    #[describe("L1 reorg count")]
    l1_reorg_count: counter,
    #[describe("Derivation pipeline L1 origin")]
    derivation_l1_origin: counter,
    #[describe("Critical errors in the derivation pipeline")]
    derivation_critical_errors: counter,
    #[describe("Tracks sequencer state flags")]
    #[label(active)]
    #[label(recovery)]
    sequencer_state: gauge,
    #[describe("Duration of the sequencer attributes builder")]
    sequencer_attributes_build_duration: gauge,
    #[describe("Duration of the sequencer block building start task")]
    sequencer_block_building_start_task_duration: gauge,
    #[describe("Duration of the sequencer block building seal task")]
    sequencer_block_building_seal_task_duration: gauge,
    #[describe("Duration of the sequencer conductor commitment")]
    sequencer_conductor_commitment_duration: gauge,
    #[describe("Total count of sequenced transactions")]
    sequencer_total_transactions_sequenced: counter,
    #[describe("Sequencer seal step retries by step")]
    #[label(step)]
    sequencer_seal_step_retries_total: counter,
    #[describe("Sequencer seal step duration by step")]
    #[label(step)]
    sequencer_seal_step_duration: gauge,
    #[describe("Seal errors by fatality")]
    #[label(fatal)]
    sequencer_seal_errors_total: counter,
    #[describe("Sequencer start rejections by reason")]
    #[label(reason)]
    sequencer_start_rejected_total: counter,
    #[describe("Deferred stop_sequencer responses due to in-flight seal pipeline")]
    sequencer_stop_deferred_total: counter,
    #[describe("Blocks sequenced in recovery mode")]
    sequencer_recovery_mode_blocks_total: counter,
    #[describe("Empty blocks produced due to sequencer drift threshold")]
    sequencer_drift_empty_blocks_total: counter,
    #[describe("Pre-built payloads discarded because the unsafe head advanced past their parent")]
    sequencer_stale_build_discarded_total: counter,
}

impl Metrics {
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

    /// Initializes metrics to `0` so they can be queried immediately by consumers of prometheus
    /// metrics.
    pub fn zero() {
        Self::l1_reorg_count().absolute(0);
        Self::derivation_critical_errors().absolute(0);
        Self::sequencer_total_transactions_sequenced().absolute(0);

        Self::sequencer_seal_step_retries_total("conductor").absolute(0);
        Self::sequencer_seal_step_retries_total("gossip").absolute(0);
        Self::sequencer_seal_step_retries_total("insert").absolute(0);
        Self::sequencer_seal_errors_total("true").absolute(0);
        Self::sequencer_seal_errors_total("false").absolute(0);
        Self::sequencer_start_rejected_total("not_leader").absolute(0);
        Self::sequencer_start_rejected_total("leadership_check_failed").absolute(0);
        Self::sequencer_stop_deferred_total().absolute(0);
        Self::sequencer_recovery_mode_blocks_total().absolute(0);
        Self::sequencer_drift_empty_blocks_total().absolute(0);
        Self::sequencer_stale_build_discarded_total().absolute(0);
    }
}
