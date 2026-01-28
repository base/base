use std::sync::Arc;

use alloy_eips::BlockId;
use alloy_primitives::B256;
use anyhow::Result;
use async_trait::async_trait;
use hokulea_host_bin::cfg::SingleChainHostWithEigenDA;
use op_succinct_host_utils::{fetcher::OPSuccinctDataFetcher, host::OPSuccinctHost};

use crate::witness_generator::EigenDAWitnessGenerator;

#[derive(Clone)]
pub struct EigenDAOPSuccinctHost {
    pub fetcher: Arc<OPSuccinctDataFetcher>,
    pub witness_generator: Arc<EigenDAWitnessGenerator>,
}

#[async_trait]
impl OPSuccinctHost for EigenDAOPSuccinctHost {
    type Args = SingleChainHostWithEigenDA;
    type WitnessGenerator = EigenDAWitnessGenerator;

    fn witness_generator(&self) -> &Self::WitnessGenerator {
        &self.witness_generator
    }

    async fn fetch(
        &self,
        l2_start_block: u64,
        l2_end_block: u64,
        l1_head_hash: Option<B256>,
        safe_db_fallback: bool,
    ) -> Result<SingleChainHostWithEigenDA> {
        // If l1_head_hash is not provided, calculate a safe L1 head
        let l1_head = match l1_head_hash {
            Some(hash) => hash,
            None => {
                self.calculate_safe_l1_head(&self.fetcher, l2_end_block, safe_db_fallback).await?
            }
        };

        let host = self.fetcher.get_host_args(l2_start_block, l2_end_block, l1_head).await?;

        let eigenda_proxy_address = std::env::var("EIGENDA_PROXY_ADDRESS").ok();
        Ok(SingleChainHostWithEigenDA { kona_cfg: host, eigenda_proxy_address, verbose: 1 })
    }

    fn get_l1_head_hash(&self, args: &Self::Args) -> Option<B256> {
        Some(args.kona_cfg.l1_head)
    }

    async fn get_finalized_l2_block_number(
        &self,
        fetcher: &OPSuccinctDataFetcher,
        _: u64,
    ) -> Result<Option<u64>> {
        let finalized_l2_block_number = fetcher.get_l2_header(BlockId::finalized()).await?;
        Ok(Some(finalized_l2_block_number.number))
    }

    async fn calculate_safe_l1_head(
        &self,
        fetcher: &OPSuccinctDataFetcher,
        l2_end_block: u64,
        safe_db_fallback: bool,
    ) -> Result<B256> {
        // For EigenDA, use a similar approach to Ethereum DA with a conservative offset.
        let (_, l1_head_number) = fetcher.get_l1_head(l2_end_block, safe_db_fallback).await?;

        // Add a buffer for EigenDA similar to Ethereum DA.
        let l1_head_number = l1_head_number + 20;

        // Ensure we don't exceed the finalized L1 header.
        let finalized_l1_header = fetcher.get_l1_header(BlockId::finalized()).await?;
        let safe_l1_head_number = std::cmp::min(l1_head_number, finalized_l1_header.number);

        Ok(fetcher.get_l1_header(safe_l1_head_number.into()).await?.hash_slow())
    }
}

impl EigenDAOPSuccinctHost {
    pub fn new(fetcher: Arc<OPSuccinctDataFetcher>) -> Self {
        Self { fetcher, witness_generator: Arc::new(EigenDAWitnessGenerator {}) }
    }
}
