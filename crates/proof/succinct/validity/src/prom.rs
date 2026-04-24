use base_proof_succinct_host_utils::metrics::MetricsGauge;
use strum::EnumMessage;
use strum_macros::{Display, EnumIter};

/// Prometheus gauge metrics for the validity proposer.
#[derive(Debug, Clone, Copy, Display, EnumIter, EnumMessage)]
pub enum ValidityGauge {
    /// Number of proofs currently unrequested.
    #[strum(
        serialize = "succinct_current_unrequested_proofs",
        message = "Number of proofs currently unrequested"
    )]
    CurrentUnrequestedProofs,
    /// Number of proofs currently being proved.
    #[strum(
        serialize = "succinct_current_proving_proofs",
        message = "Number of proofs currently being proved"
    )]
    CurrentProvingProofs,
    /// Number of proofs currently in witness generation.
    #[strum(
        serialize = "succinct_current_witnessgen_proofs",
        message = "Number of proofs currently in witness generation"
    )]
    CurrentWitnessgenProofs,
    /// Number of proofs currently being executed.
    #[strum(
        serialize = "succinct_current_execute_proofs",
        message = "Number of proofs currently being executed"
    )]
    CurrentExecuteProofs,

    /// Highest proven contiguous block number.
    #[strum(
        serialize = "succinct_highest_proven_contiguous_block",
        message = "Highest proven contiguous block"
    )]
    HighestProvenContiguousBlock,
    /// Latest L2 block number from the contract.
    #[strum(
        serialize = "succinct_latest_contract_l2_block",
        message = "Latest L2 block number from the contract"
    )]
    LatestContractL2Block,
    /// L2 unsafe head block number.
    #[strum(serialize = "succinct_l2_unsafe_head_block", message = "L2 unsafe head block number")]
    L2UnsafeHeadBlock,
    /// L2 finalized block number.
    #[strum(serialize = "succinct_l2_finalized_block", message = "L2 finalized block number")]
    L2FinalizedBlock,
    /// Minimum block number required to prove for aggregation.
    #[strum(
        serialize = "succinct_min_block_to_prove_to_agg",
        message = "Minimum block number required to prove for aggregation"
    )]
    MinBlockToProveToAgg,
    /// Number of proof request retries.
    #[strum(
        serialize = "succinct_proof_request_retry_count",
        message = "Number of proof request retries"
    )]
    ProofRequestRetryCount,

    /// Total error count.
    #[strum(serialize = "succinct_total_error_count", message = "Number of total errors")]
    TotalErrorCount,
    /// Number of proof request timeout errors.
    #[strum(
        serialize = "succinct_proof_request_timeout_error_count",
        message = "Number of proof request timeout errors"
    )]
    ProofRequestTimeoutErrorCount,
    /// Number of retry errors.
    #[strum(serialize = "succinct_retry_error_count", message = "Number of retry errors")]
    RetryErrorCount,
    /// Number of witness generation errors.
    #[strum(
        serialize = "succinct_witnessgen_error_count",
        message = "Number of witness generation errors"
    )]
    WitnessgenErrorCount,
    /// Number of execution errors.
    #[strum(serialize = "succinct_execution_error_count", message = "Number of execution errors")]
    ExecutionErrorCount,
    /// Number of range proof request errors.
    #[strum(
        serialize = "succinct_range_proof_request_error_count",
        message = "Number of range proof request errors"
    )]
    RangeProofRequestErrorCount,
    /// Number of aggregation proof request errors.
    #[strum(
        serialize = "succinct_agg_proof_request_error_count",
        message = "Number of aggregation proof request errors"
    )]
    AggProofRequestErrorCount,
    /// Number of aggregation proof validation errors.
    #[strum(
        serialize = "succinct_agg_proof_validation_error_count",
        message = "Number of aggregation proof validation errors"
    )]
    AggProofValidationErrorCount,
    /// Number of relay aggregation proof errors.
    #[strum(
        serialize = "succinct_relay_agg_proof_error_count",
        message = "Number of relay aggregation proof errors"
    )]
    RelayAggProofErrorCount,
    /// Number of network prover call timeouts.
    #[strum(
        serialize = "succinct_network_call_timeout_count",
        message = "Number of network prover call timeouts"
    )]
    NetworkCallTimeoutCount,
}

impl MetricsGauge for ValidityGauge {}
