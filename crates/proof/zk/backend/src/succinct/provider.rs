//! Witness generation for Succinct ZK proving backends.

use std::{error::Error, fmt, sync::Arc};

use alloy_primitives::B256;
use base_proof_succinct_ethereum_host_utils::host::SingleChainOPSuccinctHost;
use base_proof_succinct_host_utils::{
    fetcher::OPSuccinctDataFetcher, host::OPSuccinctHost, witness_generation::WitnessGenerator,
};
use sp1_sdk::SP1Stdin;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::succinct::{L1HeadCalculator, L1HeadError};

/// Inputs to [`OpSuccinctWitnessProvider::generate_witness`].
#[derive(Debug, Clone, Copy)]
pub struct WitnessParams<'a> {
    /// First L2 block in the range, inclusive.
    pub start_block: u64,
    /// Block past the last L2 block in the range.
    pub end_block: u64,
    /// Source for the L1 head hash used by the Succinct host.
    pub l1_head: L1HeadSource<'a>,
    /// Number of L2 blocks between sampled intermediate output roots.
    pub intermediate_root_interval: u64,
}

/// Source used to select the L1 head hash for witness generation.
#[derive(Debug, Clone, Copy)]
pub enum L1HeadSource<'a> {
    /// Use this exact L1 head hash.
    Pinned(B256),
    /// Try `SafeDB`, then fall back to sequence-window calculation.
    SafeDbWithFallback {
        /// Sequence-window size used for L1-head fallback.
        sequence_window: u64,
        /// L1 execution-layer RPC URL, used for sequence-window fallback.
        l1_node_url: &'a str,
        /// Base consensus-layer RPC URL, used for sequence-window fallback.
        base_consensus_url: &'a str,
    },
}

/// Errors raised while generating Succinct witness stdin.
#[derive(Debug, Error)]
pub enum WitnessError {
    /// Fetching host arguments with a caller-pinned L1 head failed.
    #[error("failed to fetch Succinct host args with caller-provided l1_head")]
    PinnedHostFetch {
        /// Underlying Succinct host error.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
    /// `SafeDB` failed, then sequence-window L1-head calculation also failed.
    #[error("failed to fetch Succinct host args via SafeDB and sequence-window fallback")]
    SafeDbFallbackL1Head {
        /// `SafeDB` host fetch error.
        safe_db_source: Box<dyn Error + Send + Sync>,
        /// Sequence-window L1-head calculation error.
        #[source]
        l1_head_source: L1HeadError,
    },
    /// `SafeDB` failed, then fetching host arguments with the fallback L1 head also failed.
    #[error("failed to fetch Succinct host args via SafeDB and fallback l1_head")]
    SafeDbFallbackHostFetch {
        /// `SafeDB` host fetch error.
        safe_db_source: Box<dyn Error + Send + Sync>,
        /// Fallback host fetch error.
        #[source]
        fallback_source: Box<dyn Error + Send + Sync>,
    },
    /// Running the Succinct host failed.
    #[error("failed to run Succinct host")]
    HostRun {
        /// Underlying Succinct host error.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
    /// Converting the generated witness into SP1 stdin failed.
    #[error("failed to build SP1 stdin from Succinct witness")]
    Stdin {
        /// Underlying witness conversion error.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
}

/// Provider wrapping the Succinct host for witness generation.
#[derive(Clone)]
pub struct OpSuccinctWitnessProvider {
    host: Arc<SingleChainOPSuccinctHost>,
}

impl fmt::Debug for OpSuccinctWitnessProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpSuccinctWitnessProvider").finish_non_exhaustive()
    }
}

impl OpSuccinctWitnessProvider {
    /// Create a new provider with an initialized host.
    pub fn new(fetcher: Arc<OPSuccinctDataFetcher>) -> Self {
        info!("initializing Succinct witness provider with Ethereum DA");
        let host = Arc::new(SingleChainOPSuccinctHost::new(fetcher));
        Self { host }
    }

    /// Generate witness stdin for a block range.
    ///
    /// When `params.l1_head` is pinned, that hash is used directly. Otherwise
    /// this first tries the Succinct host's `SafeDB` path, then falls back to
    /// the configured sequence-window calculation.
    pub async fn generate_witness(
        &self,
        params: WitnessParams<'_>,
    ) -> Result<SP1Stdin, WitnessError> {
        let WitnessParams { start_block, end_block, l1_head, intermediate_root_interval } = params;

        info!(
            start_block = start_block,
            end_block = end_block,
            l1_head = ?l1_head,
            "starting witness generation"
        );

        let host_args = match l1_head {
            L1HeadSource::Pinned(hash) => {
                info!(hash = %hash, "using caller-provided l1_head");
                self.host
                    .fetch(start_block, end_block, Some(hash), intermediate_root_interval, false)
                    .await
                    .map_err(|source| WitnessError::PinnedHostFetch {
                        source: source.into_boxed_dyn_error(),
                    })?
            }
            L1HeadSource::SafeDbWithFallback {
                sequence_window,
                l1_node_url,
                base_consensus_url,
            } => match self
                .host
                .fetch(start_block, end_block, None, intermediate_root_interval, false)
                .await
            {
                Ok(args) => {
                    info!("l1 head calculated via SafeDB");
                    args
                }
                Err(safe_db_err) => {
                    warn!(
                        error = %safe_db_err,
                        sequence_window = sequence_window,
                        "SafeDB unavailable, falling back to sequence_window"
                    );
                    let (_l1_head_block_num, l1_head_hash) =
                        match L1HeadCalculator::calculate_l1_head(
                            l1_node_url,
                            base_consensus_url,
                            end_block,
                            sequence_window,
                        )
                        .await
                        {
                            Ok(l1_head) => l1_head,
                            Err(l1_head_source) => {
                                return Err(WitnessError::SafeDbFallbackL1Head {
                                    safe_db_source: safe_db_err.into_boxed_dyn_error(),
                                    l1_head_source,
                                });
                            }
                        };
                    info!(l1_head_hash = %l1_head_hash, "l1 head via sequence_window fallback");
                    self.host
                        .fetch(
                            start_block,
                            end_block,
                            Some(l1_head_hash),
                            intermediate_root_interval,
                            false,
                        )
                        .await
                        .map_err(|fallback_source| WitnessError::SafeDbFallbackHostFetch {
                            safe_db_source: safe_db_err.into_boxed_dyn_error(),
                            fallback_source: fallback_source.into_boxed_dyn_error(),
                        })?
                }
            },
        };

        debug!(host_args = ?host_args, "host args fetched");

        let witness =
            self.host.run(&host_args).await.map_err(|source| WitnessError::HostRun {
                source: source.into_boxed_dyn_error(),
            })?;
        let stdin = self
            .host
            .witness_generator()
            .get_sp1_stdin(witness)
            .map_err(|source| WitnessError::Stdin { source: source.into_boxed_dyn_error() })?;

        info!(start_block = start_block, end_block = end_block, "witness generation completed");

        Ok(stdin)
    }
}
