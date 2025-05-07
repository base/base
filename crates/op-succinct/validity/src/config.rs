use alloy_primitives::{Address, B256};
use alloy_provider::{Network, Provider};
use op_succinct_host_utils::{
    DisputeGameFactory::DisputeGameFactoryInstance as DisputeGameFactoryContract,
    OPSuccinctL2OutputOracle::OPSuccinctL2OutputOracleInstance as OPSuccinctL2OOContract,
};
use sp1_sdk::{network::FulfillmentStrategy, SP1ProofMode, SP1ProvingKey, SP1VerifyingKey};
use std::sync::Arc;

pub struct ContractConfig<P, N>
where
    P: Provider<N> + 'static,
    N: Network,
{
    pub l2oo_address: Address,
    pub dgf_address: Address,
    pub l2oo_contract: OPSuccinctL2OOContract<P, N>,
    pub dgf_contract: DisputeGameFactoryContract<P, N>,
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

pub struct RequesterConfig {
    pub l1_chain_id: i64,
    pub l2_chain_id: i64,
    // The address being committed to when generating the aggregation proof to prevent
    // front-running attacks. This should be the same address that is being used to send
    // `proposeL2Output` transactions.
    pub prover_address: Address,
    pub l2oo_address: Address,
    pub dgf_address: Address,
    pub range_proof_interval: u64,
    pub submission_interval: u64,
    pub max_concurrent_witness_gen: u64,
    pub max_concurrent_proof_requests: u64,
    pub range_proof_strategy: FulfillmentStrategy,
    pub agg_proof_strategy: FulfillmentStrategy,
    pub agg_proof_mode: SP1ProofMode,
    pub mock: bool,
    /// Whether to fallback to timestamp-based L1 head estimation even though SafeDB is not
    /// activated for op-node.
    pub safe_db_fallback: bool,
}
