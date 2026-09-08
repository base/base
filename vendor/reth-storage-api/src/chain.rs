use alloc::vec::Vec;

use alloy_consensus::Header;
use alloy_hardforks::EthereumHardforks;
use base_common_consensus::{BaseBlockBody, BaseTxEnvelope};
use base_execution_chainspec::ChainSpecProvider;
use reth_storage_errors::provider::ProviderResult;

/// Reconstructs Base block bodies from transactions and the chain's fork schedule.
#[derive(Debug, Default, Clone, Copy)]
pub struct BaseBodyStorage;

impl BaseBodyStorage {
    /// Assembles bodies with empty ommers and fork-dependent empty withdrawals.
    pub fn read_block_bodies<Provider>(
        provider: &Provider,
        inputs: Vec<(&Header, Vec<BaseTxEnvelope>)>,
    ) -> ProviderResult<Vec<BaseBlockBody>>
    where
        Provider: ChainSpecProvider,
    {
        let chain_spec = provider.chain_spec();
        Ok(inputs
            .into_iter()
            .map(|(header, transactions)| BaseBlockBody {
                transactions,
                ommers: Vec::new(),
                withdrawals: chain_spec
                    .is_shanghai_active_at_timestamp(header.timestamp)
                    .then(Default::default),
            })
            .collect())
    }
}
