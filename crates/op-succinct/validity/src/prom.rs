use strum::EnumMessage;
use strum_macros::{Display, EnumIter};

use op_succinct_host_utils::metrics::MetricsGauge;

// Define an enum for all validity metrics gauges.
#[derive(Debug, Clone, Copy, Display, EnumIter, EnumMessage)]
pub enum ValidityGauge {
    // Proof status gauges
    #[strum(
        serialize = "succinct_current_unrequested_proofs",
        message = "Number of proofs currently unrequested"
    )]
    CurrentUnrequestedProofs,
    #[strum(
        serialize = "succinct_current_proving_proofs",
        message = "Number of proofs currently being proved"
    )]
    CurrentProvingProofs,
    #[strum(
        serialize = "succinct_current_witnessgen_proofs",
        message = "Number of proofs currently in witness generation"
    )]
    CurrentWitnessgenProofs,
    #[strum(
        serialize = "succinct_current_execute_proofs",
        message = "Number of proofs currently being executed"
    )]
    CurrentExecuteProofs,

    // Proposer gauges
    #[strum(
        serialize = "succinct_highest_proven_contiguous_block",
        message = "Highest proven contiguous block"
    )]
    HighestProvenContiguousBlock,
    #[strum(
        serialize = "succinct_latest_contract_l2_block",
        message = "Latest L2 block number from the contract"
    )]
    LatestContractL2Block,
    #[strum(serialize = "succinct_l2_unsafe_head_block", message = "L2 unsafe head block number")]
    L2UnsafeHeadBlock,
    #[strum(serialize = "succinct_l2_finalized_block", message = "L2 finalized block number")]
    L2FinalizedBlock,
    #[strum(
        serialize = "succinct_min_block_to_prove_to_agg",
        message = "Minimum block number required to prove for aggregation"
    )]
    MinBlockToProveToAgg,
    #[strum(
        serialize = "succinct_proof_request_retry_count",
        message = "Number of proof request retries"
    )]
    ProofRequestRetryCount,

    // Error gauges
    #[strum(serialize = "succinct_total_error_count", message = "Number of total errors")]
    TotalErrorCount,
    #[strum(
        serialize = "succinct_proof_request_timeout_error_count",
        message = "Number of proof request timeout errors"
    )]
    ProofRequestTimeoutErrorCount,
    #[strum(serialize = "succinct_retry_error_count", message = "Number of retry errors")]
    RetryErrorCount,
    #[strum(
        serialize = "succinct_witnessgen_error_count",
        message = "Number of witness generation errors"
    )]
    WitnessgenErrorCount,
    #[strum(serialize = "succinct_execution_error_count", message = "Number of execution errors")]
    ExecutionErrorCount,
    #[strum(
        serialize = "succinct_range_proof_request_error_count",
        message = "Number of range proof request errors"
    )]
    RangeProofRequestErrorCount,
    #[strum(
        serialize = "succinct_agg_proof_request_error_count",
        message = "Number of aggregation proof request errors"
    )]
    AggProofRequestErrorCount,
    #[strum(
        serialize = "succinct_agg_proof_validation_error_count",
        message = "Number of aggregation proof validation errors"
    )]
    AggProofValidationErrorCount,
    #[strum(
        serialize = "succinct_relay_agg_proof_error_count",
        message = "Number of relay aggregation proof errors"
    )]
    RelayAggProofErrorCount,
    #[strum(
        serialize = "succinct_network_call_timeout_count",
        message = "Number of network prover call timeouts"
    )]
    NetworkCallTimeoutCount,
}

impl MetricsGauge for ValidityGauge {}
