pub mod default;

use crate::fetcher::OPSuccinctDataFetcher;
use alloy_primitives::B256;
use anyhow::Result;
use async_trait::async_trait;
use default::SingleChainOPSuccinctHost;
use kona_preimage::{HintWriter, NativeChannel, OracleReader};
use op_succinct_client_utils::client::run_opsuccinct_client;
use op_succinct_client_utils::precompiles::zkvm_handle_register;
use op_succinct_client_utils::{InMemoryOracle, StoreOracle};
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

    /// Fetch the host arguments. Optionally supply an L1 block number which is used as the L1 origin.
    async fn fetch(
        &self,
        l2_start_block: u64,
        l2_end_block: u64,
        l1_head_hash: Option<B256>,
    ) -> Result<Self::Args>;
}

/// Initialize the host.
///
/// In the future, there will be a feature gated function to initialize the host (ex. for Alt-DA).
pub fn initialize_host(fetcher: Arc<OPSuccinctDataFetcher>) -> Arc<SingleChainOPSuccinctHost> {
    Arc::new(SingleChainOPSuccinctHost::new(fetcher))
}
