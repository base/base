//! Witness generation for OP-Succinct proving.

use std::{fmt, sync::Arc};

use alloy_primitives::B256;
use anyhow::Result;
use base_proof_succinct_ethereum_host_utils::host::SingleChainOPSuccinctHost;
use base_proof_succinct_host_utils::{
    fetcher::OPSuccinctDataFetcher, host::OPSuccinctHost, witness_generation::WitnessGenerator,
};
use sp1_sdk::SP1Stdin;
use tracing::{debug, info};

use crate::backends::utils::L1HeadCalculator;

/// Provider wrapping the OP Succinct host for witness generation and proving.
#[derive(Clone)]
pub struct OpSuccinctProvider {
    host: Arc<SingleChainOPSuccinctHost>,
}

impl fmt::Debug for OpSuccinctProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpSuccinctProvider").finish_non_exhaustive()
    }
}

impl OpSuccinctProvider {
    /// Create a new provider with an initialized host.
    pub fn new(fetcher: Arc<OPSuccinctDataFetcher>) -> Self {
        info!("initializing OP-Succinct provider with Ethereum DA");
        let host = Arc::new(SingleChainOPSuccinctHost::new(fetcher));
        Self { host }
    }

    /// Get a reference to the underlying data fetcher (used for aggregation L1
    /// header queries).
    pub fn fetcher(&self) -> &Arc<OPSuccinctDataFetcher> {
        &self.host.fetcher
    }

    /// Generate witness (`SP1Stdin`) for a block range.
    ///
    /// When `l1_head` is `Some`, the provided hash is used directly (bypassing
    /// `SafeDB` and sequence-window calculation). When `None`, tries `SafeDB`
    /// first, then falls back to sequence-window.
    pub async fn generate_witness(
        &self,
        start_block: u64,
        end_block: u64,
        sequence_window: u64,
        l1_node_url: &str,
        base_consensus_url: &str,
        l1_head: Option<B256>,
    ) -> Result<SP1Stdin> {
        info!(
            start_block = start_block,
            end_block = end_block,
            sequence_window = sequence_window,
            l1_head = ?l1_head,
            "starting witness generation"
        );

        let host_args = match l1_head {
            Some(hash) => {
                info!(hash = %hash, "using caller-provided l1_head");
                self.host.fetch(start_block, end_block, Some(hash), false).await?
            }
            None => match self.host.fetch(start_block, end_block, None, false).await {
                Ok(args) => {
                    info!("l1 head calculated via SafeDB (optimism_safeHeadAtL1Block)");
                    args
                }
                Err(safe_db_err) => {
                    info!(
                        error = %safe_db_err,
                        sequence_window = sequence_window,
                        "SafeDB unavailable, falling back to sequence_window"
                    );
                    let (_l1_head_block_num, l1_head_hash) = L1HeadCalculator::calculate_l1_head(
                        l1_node_url,
                        base_consensus_url,
                        end_block,
                        sequence_window,
                    )
                    .await?;
                    info!(
                        l1_head_hash = %l1_head_hash,
                        "l1 head via sequence_window fallback"
                    );
                    self.host.fetch(start_block, end_block, Some(l1_head_hash), false).await?
                }
            },
        };

        debug!(host_args = ?host_args, "host args fetched");

        let witness = self.host.run(&host_args).await?;

        let stdin = self.host.witness_generator().get_sp1_stdin(
            witness,
            base_proof_succinct_client_utils::client::DEFAULT_INTERMEDIATE_ROOT_INTERVAL,
        )?;

        info!(start_block = start_block, end_block = end_block, "witness generation completed");

        Ok(stdin)
    }
}
