//! Metrics for the node service

base_metrics::define_metrics! {
    base_node
    #[describe("L1 reorg count")]
    l1_reorg_count: counter,
    #[describe("Derivation pipeline L1 origin")]
    derivation_l1_origin: counter,
    #[describe("Critical errors in the derivation pipeline")]
    derivation_critical_errors: counter,
    #[describe("Advisory follow-mode L1 origin check failures by reason")]
    #[label(name = "reason", default = ["unavailable", "fetch_failed", "not_canonical"])]
    follow_l1_origin_check_failures_total: counter,
    #[describe("Wall-clock duration of a single derivation pipeline step() call")]
    derivation_pipeline_step_duration_seconds: histogram,
    #[describe("Wall-clock duration the derivation actor waits for an inbound request")]
    derivation_actor_inbound_recv_wait_duration_seconds: histogram,
    #[describe("Tracks sequencer state flags")]
    #[label(active)]
    #[label(recovery)]
    sequencer_state: gauge,
    #[describe("Sequencer L1 origin RPC calls by method and outcome")]
    #[label(name = "method", default = ["block_by_hash", "block_by_number", "block_receipts"])]
    #[label(name = "outcome", default = ["success", "not_found", "timeout", "error"])]
    sequencer_l1_origin_rpc_calls_total: counter,
    #[describe("Wall-clock duration of sequencer L1 origin RPC calls in seconds")]
    #[label(name = "method", default = ["block_by_hash", "block_by_number", "block_receipts"])]
    #[label(name = "outcome", default = ["success", "not_found", "timeout", "error"])]
    sequencer_l1_origin_rpc_duration_seconds: histogram,
    #[describe("Duration of the sequencer attributes builder")]
    sequencer_attributes_build_duration: histogram,
    #[describe("Duration of the sequencer block building start task")]
    sequencer_block_building_start_task_duration: histogram,
    #[describe("Duration of the sequencer block building seal task")]
    sequencer_block_building_seal_task_duration: histogram,
    #[describe("Total count of sequenced transactions")]
    sequencer_total_transactions_sequenced: counter,
    #[describe("Sequencer seal step retries by step")]
    #[label(name = "step", default = ["conductor", "gossip", "insert"])]
    sequencer_seal_step_retries_total: counter,
    #[describe("Sequencer seal step duration by step")]
    #[label(name = "step", default = ["conductor", "gossip", "insert"])]
    sequencer_seal_step_duration: histogram,
    #[describe("Wall-clock duration between successive successful seal completions (Ok(true) returns)")]
    sequencer_block_to_block_duration: histogram,
    #[describe("Wall-clock drift between the build-ticker target time and the actual fire time (>= 0; clamped to 0 when the ticker fires early)")]
    sequencer_ticker_drift_seconds: histogram,
    #[describe("Wall-clock duration of the full seal pipeline (conductor commit → gossip → engine insert), measured from PayloadSealer construction (after the EL seal response) until step() returns Ok(true). Excludes the EL build idle wait and the EL seal request.")]
    sequencer_seal_pipeline_duration: histogram,
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
    #[describe("L1 origin lookups served from the selected-origin slot")]
    #[label(name = "kind", default = ["header", "receipts"])]
    sequencer_l1_origin_buffer_hits_total: counter,
    #[describe("L1 origin lookups that missed the selected-origin slot")]
    #[label(name = "kind", default = ["header", "receipts"])]
    sequencer_l1_origin_buffer_misses_total: counter,
    #[describe("Pre-built payloads discarded because the unsafe head advanced past their parent")]
    sequencer_stale_build_discarded_total: counter,
    #[describe("Configured verifier L1 confirmation depth")]
    l1_verifier_confs_depth: gauge,
    #[describe("L1 block number forwarded to derivation after verifier confirmation delay")]
    l1_verifier_derivation_head: counter,
    #[describe("Failed attempts to fetch a delayed L1 block for verifier confirmation")]
    l1_verifier_delayed_fetch_errors: counter,
}
