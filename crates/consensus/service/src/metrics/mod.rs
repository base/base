//! Metrics for the node service

base_metrics::define_metrics! {
    base_node
    #[describe("L1 reorg count")]
    l1_reorg_count: counter,
    #[describe("Derivation pipeline L1 origin")]
    derivation_l1_origin: counter,
    #[describe("Critical errors in the derivation pipeline")]
    derivation_critical_errors: counter,
    #[describe("Wall-clock duration of a single derivation pipeline step() call")]
    derivation_pipeline_step_duration_seconds: histogram,
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
    #[label(name = "step", default = ["conductor", "gossip", "insert"])]
    sequencer_seal_step_retries_total: counter,
    #[describe("Sequencer seal step duration by step")]
    #[label(step)]
    sequencer_seal_step_duration: gauge,
    #[describe("Seal errors by fatality")]
    #[label(name = "fatal", default = ["true", "false"])]
    sequencer_seal_errors_total: counter,
    #[describe("Sequencer start rejections by reason")]
    #[label(name = "reason", default = ["not_leader", "leadership_check_failed"])]
    sequencer_start_rejected_total: counter,
    #[describe("Deferred stop_sequencer responses due to in-flight seal pipeline")]
    sequencer_stop_deferred_total: counter,
    #[describe("Blocks sequenced in recovery mode")]
    sequencer_recovery_mode_blocks_total: counter,
    #[describe("Empty blocks produced due to sequencer drift threshold")]
    sequencer_drift_empty_blocks_total: counter,
    #[describe("Pre-built payloads discarded because the unsafe head advanced past their parent")]
    sequencer_stale_build_discarded_total: counter,
    #[describe("Configured verifier L1 confirmation depth")]
    l1_verifier_confs_depth: gauge,
    #[describe("L1 block number forwarded to derivation after verifier confirmation delay")]
    l1_verifier_derivation_head: counter,
    #[describe("Failed attempts to fetch a delayed L1 block for verifier confirmation")]
    l1_verifier_delayed_fetch_errors: counter,
}
