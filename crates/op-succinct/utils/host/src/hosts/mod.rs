use crate::fetcher::OPSuccinctDataFetcher;
use alloy_primitives::B256;
use anyhow::Result;
use async_trait::async_trait;
use kona_preimage::{HintWriter, NativeChannel, OracleReader};
use op_succinct_client_utils::{
    client::run_opsuccinct_client, precompiles::zkvm_handle_register, InMemoryOracle, StoreOracle,
};
use std::sync::Arc;

#[async_trait]
pub trait OPSuccinctHost: Send + Sync + 'static {
    type Args: Send + Sync + 'static + Clone;

    /// Run the host and client program.
    ///
    /// Returns the in-memory oracle which can be supplied to the zkVM.
    async fn run(&self, args: &Self::Args) -> Result<InMemoryOracle>;

    /// Run the witness generation client.
    async fn run_witnessgen_client(
        preimage_chan: NativeChannel,
        hint_chan: NativeChannel,
    ) -> Result<InMemoryOracle> {
        let oracle = Arc::new(StoreOracle::new(
            OracleReader::new(preimage_chan),
            HintWriter::new(hint_chan),
        ));
        run_opsuccinct_client(oracle.clone(), Some(zkvm_handle_register)).await?;
        let in_memory_oracle = InMemoryOracle::populate_from_store(oracle.as_ref())?;
        Ok(in_memory_oracle)
    }

    /// Fetch the host arguments.
    ///
    /// Parameters:
    /// - `l2_start_block`: The starting L2 block number
    /// - `l2_end_block`: The ending L2 block number
    /// - `l1_head_hash`: Optionally supplied L1 head block hash used as the L1 origin.
    /// - `safe_db_fallback`: Optionally supplied flag to indicate whether to fallback to
    ///   timestamp-based L1 head estimation when SafeDB is not available. This is optional to
    ///   support abstraction across different node implementations.
    async fn fetch(
        &self,
        l2_start_block: u64,
        l2_end_block: u64,
        l1_head_hash: Option<B256>,
        safe_db_fallback: Option<bool>,
    ) -> Result<Self::Args>;

    /// Get the L1 head hash from the host args.
    fn get_l1_head_hash(&self, args: &Self::Args) -> Option<B256>;

    /// Get the finalized L2 block number. This is used to determine the highest block that can be
    /// included in a range proof.
    ///
    /// For ETH DA, this is the finalized L2 block number.
    /// For Celestia, this is the highest L2 block included in the latest Blobstream commitment.
    ///
    /// The latest proposed block number is assumed to be the highest block number that has been
    /// successfully processed by the host.
    async fn get_finalized_l2_block_number(
        &self,
        fetcher: &OPSuccinctDataFetcher,
        latest_proposed_block_number: u64,
    ) -> Result<Option<u64>>;
}

cfg_if::cfg_if! {
    if #[cfg(feature = "celestia")] {
        mod celestia;
        use crate::hosts::celestia::CelestiaOPSuccinctHost;

        /// Initialize the Celestia host.
        pub fn initialize_host(
            fetcher: Arc<OPSuccinctDataFetcher>,
        ) -> Arc<CelestiaOPSuccinctHost> {
            Arc::new(CelestiaOPSuccinctHost::new(fetcher))
        }
    } else {
        mod default;
        use crate::hosts::default::SingleChainOPSuccinctHost;

        /// Initialize the default (ETH-DA) host.
        pub fn initialize_host(
            fetcher: Arc<OPSuccinctDataFetcher>,
        ) -> Arc<SingleChainOPSuccinctHost> {
            Arc::new(SingleChainOPSuccinctHost::new(fetcher))
        }
    }
}
