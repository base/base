use alloy_primitives::{Address, B256};
use alloy_provider::Provider;
use op_succinct_host_utils::{
    DisputeGameFactory::DisputeGameFactoryInstance as DisputeGameFactoryContract,
    OPSuccinctL2OutputOracle::OPSuccinctL2OutputOracleInstance as OPSuccinctL2OOContract,
};
use sp1_sdk::{network::FulfillmentStrategy, SP1ProofMode, SP1ProvingKey, SP1VerifyingKey};
use std::sync::Arc;

pub struct ContractConfig<P>
where
    P: Provider + 'static,
{
    pub l2oo_address: Address,
    pub dgf_address: Address,
    pub l2oo_contract: OPSuccinctL2OOContract<P>,
    pub dgf_contract: DisputeGameFactoryContract<P>,
}

#[derive(Debug, Clone)]
pub struct CommitmentConfig {
    pub range_vkey_commitment: B256,
    pub agg_vkey_hash: B256,
    pub rollup_config_hash: B256,
}

#[derive(Clone)]
pub struct ProgramConfig {
    pub range_vk: Arc<SP1VerifyingKey>,
    pub range_pk: Arc<SP1ProvingKey>,
    pub agg_vk: Arc<SP1VerifyingKey>,
    pub agg_pk: Arc<SP1ProvingKey>,
    pub commitments: CommitmentConfig,
}

impl ProgramConfig {
    pub fn log(&self) {
        tracing::info!(
            range_vkey_commitment = %self.commitments.range_vkey_commitment,
            agg_vkey_hash = %self.commitments.agg_vkey_hash,
            rollup_config_hash = %self.commitments.rollup_config_hash,
            "Program configuration loaded"
        );
    }
}

pub struct RequesterConfig {
    pub l1_chain_id: i64,
    pub l2_chain_id: i64,
    pub l2oo_address: Address,
    pub dgf_address: Address,
    /// The evm gas limit for each range proof. Ranges will be split to not exceed this gas limit.
    /// If 0, will use range_proof_interval instead.
    pub evm_gas_limit: u64,
    /// The number of blocks in each range proof. Used when gas_limit is 0.
    pub range_proof_interval: u64,
    pub submission_interval: u64,
    pub max_concurrent_witness_gen: u64,
    pub max_concurrent_proof_requests: u64,
    pub range_proof_strategy: FulfillmentStrategy,
    pub agg_proof_strategy: FulfillmentStrategy,
    pub agg_proof_mode: SP1ProofMode,
    pub op_succinct_config_name_hash: B256,
    pub mock: bool,

    /// Whether to fallback to timestamp-based L1 head estimation even though SafeDB is not
    /// activated for op-node.
    pub safe_db_fallback: bool,

    /// Whether to expect NETWORK_PRIVATE_KEY to be an AWS KMS key ARN instead of a
    /// plaintext private key.
    pub use_kms_requester: bool,

    /// The maximum price per pgu for proving.
    pub max_price_per_pgu: u64,

    /// The timeout to use for proving (in seconds).
    pub proving_timeout: u64,

    /// The timeout to use for network prover calls (in seconds).
    pub network_calls_timeout: u64,

    /// The cycle limit to use for range proofs.
    pub range_cycle_limit: u64,

    /// The gas limit to use for range proofs.
    pub range_gas_limit: u64,

    /// The cycle limit to use for aggregation proofs.
    pub agg_cycle_limit: u64,

    /// The gas limit to use for aggregation proofs.
    pub agg_gas_limit: u64,

    /// The list of prover addresses that are allowed to bid on proof requests.
    pub whitelist: Option<Vec<Address>>,

    /// The minimum auction period (in seconds).
    pub min_auction_period: u64,

    /// How long to wait before cancelling a proof request that hasn't been assigned.
    pub auction_timeout: u64,
}

impl RequesterConfig {
    /// Log the configuration using structured tracing fields.
    pub fn log(&self) {
        tracing::info!(
            l1_chain_id = self.l1_chain_id,
            l2_chain_id = self.l2_chain_id,
            l2oo_address = %self.l2oo_address,
            dgf_address = %self.dgf_address,
            evm_gas_limit = self.evm_gas_limit,
            range_proof_interval = self.range_proof_interval,
            submission_interval = self.submission_interval,
            max_concurrent_witness_gen = self.max_concurrent_witness_gen,
            max_concurrent_proof_requests = self.max_concurrent_proof_requests,
            range_proof_strategy = ?self.range_proof_strategy,
            agg_proof_strategy = ?self.agg_proof_strategy,
            agg_proof_mode = ?self.agg_proof_mode,
            op_succinct_config_name_hash = %self.op_succinct_config_name_hash,
            mock = self.mock,
            safe_db_fallback = self.safe_db_fallback,
            use_kms_requester = self.use_kms_requester,
            max_price_per_pgu = self.max_price_per_pgu,
            proving_timeout = self.proving_timeout,
            network_calls_timeout = self.network_calls_timeout,
            range_cycle_limit = self.range_cycle_limit,
            range_gas_limit = self.range_gas_limit,
            agg_cycle_limit = self.agg_cycle_limit,
            agg_gas_limit = self.agg_gas_limit,
            whitelist = ?self.whitelist,
            min_auction_period = self.min_auction_period,
            auction_timeout = self.auction_timeout,
            "Validity proposer configuration loaded"
        );
    }
}
