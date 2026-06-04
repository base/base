//! Witness generation for Succinct proving.

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

/// Inputs to [`OpSuccinctProvider::generate_witness`].
///
/// Grouped so the call site reads as named fields rather than 7 positional values of similar
/// primitive types.
#[derive(Debug, Clone, Copy)]
pub struct WitnessParams<'a> {
    /// First L2 block in the range (inclusive).
    pub start_block: u64,
    /// Block past the last L2 block in the range (exclusive).
    pub end_block: u64,
    /// Sequence-window size used for L1-head fallback when `l1_head` is `None`.
    pub sequence_window: u64,
    /// L1 execution-layer RPC, used for the sequence-window fallback path.
    pub l1_node_url: &'a str,
    /// Base consensus-layer RPC, used for the sequence-window fallback path.
    pub base_consensus_url: &'a str,
    /// Optional caller-pinned L1 head hash. When `None`, `SafeDB` is tried first then
    /// sequence-window fallback.
    pub l1_head: Option<B256>,
    /// Number of L2 blocks between sampled intermediate output roots.
    pub intermediate_root_interval: u64,
}

/// Provider wrapping the Succinct host for witness generation and proving.
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
        info!("initializing Succinct provider with Ethereum DA");
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
    /// When `params.l1_head` is `Some`, the provided hash is used directly (bypassing `SafeDB` and
    /// sequence-window calculation). When `None`, tries `SafeDB` first, then falls back to
    /// sequence-window.
    pub async fn generate_witness(&self, params: WitnessParams<'_>) -> Result<SP1Stdin> {
        let WitnessParams {
            start_block,
            end_block,
            sequence_window,
            l1_node_url,
            base_consensus_url,
            l1_head,
            intermediate_root_interval,
        } = params;

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
                self.host
                    .fetch(start_block, end_block, Some(hash), intermediate_root_interval, false)
                    .await?
            }
            None => match self
                .host
                .fetch(start_block, end_block, None, intermediate_root_interval, false)
                .await
            {
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
                    self.host
                        .fetch(
                            start_block,
                            end_block,
                            Some(l1_head_hash),
                            intermediate_root_interval,
                            false,
                        )
                        .await?
                }
            },
        };

        debug!(host_args = ?host_args, "host args fetched");

        let witness = self.host.run(&host_args).await?;

        let stdin = self.host.witness_generator().get_sp1_stdin(witness)?;

        info!(start_block = start_block, end_block = end_block, "witness generation completed");

        Ok(stdin)
    }
}
