use std::{fmt, sync::Arc};

use alloy_eips::BlockId;
use alloy_primitives::B256;
use anyhow::Result;
use async_trait::async_trait;
use base_proof_host::{Host, HostConfig, HostError};
use base_proof_preimage::{BidirectionalChannel, Channel};
use tokio::task::JoinHandle;

use crate::{
    fetcher::OPSuccinctDataFetcher,
    witness_generation::{DefaultWitnessData, WitnessGenerator},
};

/// Starts a preimage hint/oracle server.
#[async_trait]
pub trait PreimageServerStarter {
    /// Launch the server on the given hint and preimage channels.
    async fn start_server<C>(
        &self,
        hint: C,
        preimage: C,
    ) -> Result<JoinHandle<Result<(), HostError>>, HostError>
    where
        C: Channel + Send + Sync + 'static;
}

#[async_trait]
impl PreimageServerStarter for HostConfig {
    async fn start_server<C>(
        &self,
        hint: C,
        preimage: C,
    ) -> Result<JoinHandle<Result<(), HostError>>, HostError>
    where
        C: Channel + Send + Sync + 'static,
    {
        Host::new(self.clone()).start_server(hint, preimage).await
    }
}

/// Host for Succinct proof generation with Ethereum data availability.
#[derive(Clone)]
pub struct SuccinctHost {
    /// L1/L2 data fetcher.
    pub fetcher: Arc<OPSuccinctDataFetcher>,
    witness_generator: Arc<WitnessGenerator>,
}

impl fmt::Debug for SuccinctHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SuccinctHost").finish_non_exhaustive()
    }
}

impl SuccinctHost {
    /// Create a new host from a data fetcher.
    pub fn new(fetcher: Arc<OPSuccinctDataFetcher>) -> Self {
        Self { fetcher, witness_generator: Arc::new(WitnessGenerator::new()) }
    }

    /// Return a reference to the witness generator.
    pub fn witness_generator(&self) -> &WitnessGenerator {
        &self.witness_generator
    }

    /// Fetch the host arguments.
    ///
    /// Parameters:
    /// - `l2_start_block`: The starting L2 block number.
    /// - `l2_end_block`: The ending L2 block number.
    /// - `l1_head_hash`: Optionally supplied L1 head block hash used as the L1 origin.
    /// - `intermediate_block_interval`: L2 blocks between intermediate output roots, must match
    ///   on-chain `INTERMEDIATE_BLOCK_INTERVAL` (same field committed into [`BootInfo`]).
    /// - `safe_db_fallback`: Flag to indicate whether to fallback to timestamp-based L1 head
    ///   estimation when `SafeDB` is not available.
    pub async fn fetch(
        &self,
        l2_start_block: u64,
        l2_end_block: u64,
        l1_head_hash: Option<B256>,
        intermediate_block_interval: u64,
        safe_db_fallback: bool,
    ) -> Result<HostConfig> {
        let l1_head_hash = match l1_head_hash {
            Some(hash) => hash,
            None => self.calculate_safe_l1_head(l2_end_block, safe_db_fallback).await?,
        };

        self.fetcher
            .get_host_args(l2_start_block, l2_end_block, l1_head_hash, intermediate_block_interval)
            .await
    }

    /// Run the host and client program.
    ///
    /// Returns the witness which can be supplied to the zkVM.
    pub async fn run(&self, args: &HostConfig) -> Result<DefaultWitnessData> {
        let preimage = BidirectionalChannel::new()?;
        let hint = BidirectionalChannel::new()?;

        let server_task = args.start_server(hint.host, preimage.host).await?;

        let witness = self.witness_generator.run(preimage.client, hint.client).await?;
        // Unlike the upstream, manually abort the server task, as it will hang if you wait for both
        // tasks to complete.
        server_task.abort();

        Ok(witness)
    }

    /// Get the finalized L2 block number. This is used to determine the highest block that can be
    /// included in a range proof.
    pub async fn get_finalized_l2_block_number(&self) -> Result<u64> {
        let finalized_l2_block = self.fetcher.get_l2_header(BlockId::finalized()).await?;
        Ok(finalized_l2_block.number)
    }

    /// Calculate a safe L1 head hash for the given L2 end block.
    pub async fn calculate_safe_l1_head(
        &self,
        l2_end_block: u64,
        safe_db_fallback: bool,
    ) -> Result<B256> {
        let (_, l1_head_number) = self.fetcher.get_l1_head(l2_end_block, safe_db_fallback).await?;

        // FIXME(fakedev9999): Investigate requirement for L1 head offset beyond batch posting block
        // with safe head > L2 end block.
        let l1_head_number = l1_head_number + 20;

        let finalized_l1_header = self.fetcher.get_l1_header(BlockId::finalized()).await?;
        let safe_l1_head_number = std::cmp::min(l1_head_number, finalized_l1_header.number);

        Ok(self.fetcher.get_l1_header(safe_l1_head_number.into()).await?.hash_slow())
    }
}
