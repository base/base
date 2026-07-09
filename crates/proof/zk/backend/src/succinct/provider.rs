//! Witness generation for Succinct ZK proving backends.

use std::{error::Error as StdError, fmt, sync::Arc};

use alloy_primitives::{Address, B256};
use base_l1_head::{L1HeadCalculator, L1HeadError};
use base_proof_succinct_client_utils::boot::BootInfoStruct;
use base_proof_succinct_host_utils::{
    fetcher::OPSuccinctDataFetcher, get_agg_proof_stdin, host::SuccinctHost,
};
use sp1_sdk::{SP1ProofWithPublicValues, SP1Stdin, SP1VerifyingKey};
use thiserror::Error;
use tracing::{debug, info};

/// Inputs to [`OpSuccinctWitnessProvider::generate_witness`].
#[derive(Debug, Clone, Copy)]
pub struct WitnessParams<'a> {
    /// First L2 block in the range, inclusive.
    pub start_block: u64,
    /// Last L2 block in the range, inclusive.
    pub end_block: u64,
    /// Source for the L1 head hash used by the Succinct host.
    pub l1_head: L1HeadSource<'a>,
    /// Number of L2 blocks between sampled intermediate output roots.
    pub intermediate_root_interval: u64,
}

/// Source used to select the L1 head hash for witness generation.
#[derive(Clone, Copy)]
pub enum L1HeadSource<'a> {
    /// Use this exact L1 head hash.
    Pinned(B256),
    /// Calculate the L1 head hash from the requested L2 range and sequence window.
    SequenceWindow {
        /// Sequence-window size used for L1-head calculation.
        sequence_window: u64,
        /// L1 execution-layer RPC URL, used for sequence-window calculation.
        l1_node_url: &'a str,
        /// Base consensus-layer RPC URL, used for sequence-window calculation.
        base_consensus_url: &'a str,
    },
}

impl fmt::Debug for L1HeadSource<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pinned(hash) => f.debug_tuple("Pinned").field(hash).finish(),
            Self::SequenceWindow { sequence_window, .. } => f
                .debug_struct("SequenceWindow")
                .field("sequence_window", sequence_window)
                .field("l1_node_url", &"<redacted>")
                .field("base_consensus_url", &"<redacted>")
                .finish(),
        }
    }
}

impl L1HeadSource<'_> {
    /// Returns the source variant name without exposing configured URLs.
    pub const fn variant_name(&self) -> &'static str {
        match self {
            Self::Pinned(_) => "Pinned",
            Self::SequenceWindow { .. } => "SequenceWindow",
        }
    }
}

/// Errors raised while generating Succinct witness stdin.
#[derive(Debug, Error)]
pub enum WitnessError {
    /// Fetching host arguments with a caller-pinned L1 head failed.
    #[error("failed to fetch Succinct host args with caller-provided l1_head")]
    PinnedHostFetch {
        /// Underlying Succinct host error.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    /// Sequence-window L1-head calculation failed.
    #[error("failed to calculate sequence-window l1_head")]
    SequenceWindowL1Head {
        /// Sequence-window L1-head calculation error.
        #[source]
        source: L1HeadError,
    },
    /// Fetching host arguments with a sequence-window L1 head failed.
    #[error("failed to fetch Succinct host args with sequence-window l1_head")]
    SequenceWindowHostFetch {
        /// Underlying Succinct host error.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    /// Running the Succinct host failed.
    #[error("failed to run Succinct host")]
    HostRun {
        /// Underlying Succinct host error.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    /// Converting the generated witness into SP1 stdin failed.
    #[error("failed to build SP1 stdin from Succinct witness")]
    Stdin {
        /// Underlying witness conversion error.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    /// Fetching the latest L1 checkpoint head for aggregation failed.
    #[error("failed to fetch latest L1 checkpoint head for aggregation")]
    AggregationL1Head {
        /// Underlying header fetch error.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    /// Fetching L1 header preimages for aggregation failed.
    #[error("failed to fetch L1 header preimages for aggregation")]
    AggregationHeaders {
        /// Underlying header fetch error.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    /// Building aggregation stdin failed.
    #[error("failed to build aggregation stdin")]
    AggregationStdin {
        /// Underlying aggregation stdin error.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
}

/// Provider wrapping the Succinct host for witness generation.
#[derive(Clone)]
pub struct OpSuccinctWitnessProvider {
    host: Arc<SuccinctHost>,
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
        let host = Arc::new(SuccinctHost::new(fetcher));
        Self { host }
    }

    /// Generate witness stdin for a block range.
    ///
    /// When `params.l1_head` is pinned, that hash is used directly. Otherwise
    /// the configured sequence-window calculation selects the L1 head.
    pub async fn generate_witness(
        &self,
        params: WitnessParams<'_>,
    ) -> Result<SP1Stdin, WitnessError> {
        let WitnessParams { start_block, end_block, l1_head, intermediate_root_interval } = params;

        info!(
            start_block = start_block,
            end_block = end_block,
            l1_head_source = l1_head.variant_name(),
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
            L1HeadSource::SequenceWindow { sequence_window, l1_node_url, base_consensus_url } => {
                let (l1_head_block_num, l1_head_hash) =
                    L1HeadCalculator::calculate_l1_head_from_urls(
                        l1_node_url,
                        base_consensus_url,
                        end_block,
                        sequence_window,
                    )
                    .await
                    .map_err(|source| WitnessError::SequenceWindowL1Head { source })?;
                info!(
                    l1_head_block = l1_head_block_num,
                    l1_head_hash = %l1_head_hash,
                    "l1 head calculated via sequence_window"
                );
                self.host
                    .fetch(
                        start_block,
                        end_block,
                        Some(l1_head_hash),
                        intermediate_root_interval,
                        false,
                    )
                    .await
                    .map_err(|source| WitnessError::SequenceWindowHostFetch {
                        source: source.into_boxed_dyn_error(),
                    })?
            }
        };

        debug!(start_block = start_block, end_block = end_block, "host args fetched");

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

    /// Generate aggregation stdin from a completed compressed range proof.
    pub async fn generate_aggregation_witness(
        &self,
        mut range_proof: SP1ProofWithPublicValues,
        range_vk: &SP1VerifyingKey,
        prover_address: Address,
    ) -> Result<SP1Stdin, WitnessError> {
        let boot_info: BootInfoStruct = range_proof.public_values.read();
        let boot_infos = vec![boot_info];
        let proofs = vec![range_proof.proof];

        let header =
            self.host.fetcher.get_latest_l1_head_in_batch(&boot_infos).await.map_err(|source| {
                WitnessError::AggregationL1Head { source: source.into_boxed_dyn_error() }
            })?;
        let l1_head_hash = header.hash_slow();

        let headers =
            self.host.fetcher.get_header_preimages(&boot_infos, l1_head_hash).await.map_err(
                |source| WitnessError::AggregationHeaders { source: source.into_boxed_dyn_error() },
            )?;

        info!(
            l1_head_hash = %l1_head_hash,
            num_headers = headers.len(),
            "fetched L1 headers for aggregation proof"
        );

        get_agg_proof_stdin(proofs, boot_infos, headers, range_vk, l1_head_hash, prover_address)
            .map_err(|source| WitnessError::AggregationStdin {
                source: source.into_boxed_dyn_error(),
            })
    }
}
