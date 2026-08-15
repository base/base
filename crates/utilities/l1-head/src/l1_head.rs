//! L1 head calculation for proof ranges.

use alloy_primitives::B256;
use alloy_provider::{Identity, Provider, ProviderBuilder};
use alloy_rpc_types::{BlockId, BlockNumberOrTag};
use base_common_network::Base;
use base_optimism_rpc::OptimismRollupProviderExt;
use thiserror::Error;
use tracing::debug;
use url::Url;

/// Errors raised while calculating an L1 head for a proof range.
#[derive(Debug, Error)]
pub enum L1HeadError {
    /// The configured L1 execution-layer RPC URL is invalid.
    #[error("invalid L1 execution RPC URL")]
    InvalidL1RpcUrl {
        /// URL parse error.
        #[source]
        source: url::ParseError,
    },
    /// The configured Base consensus-layer RPC URL is invalid.
    #[error("invalid Base consensus RPC URL")]
    InvalidBaseConsensusUrl {
        /// URL parse error.
        #[source]
        source: url::ParseError,
    },
    /// Fetching the L1 origin from the Base consensus endpoint failed.
    #[error("failed to fetch L1 origin from Base consensus RPC")]
    L1OriginFetch {
        /// Underlying RPC error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The desired L1 head overflowed `u64`.
    #[error("l1_origin + sequence_window overflowed u64")]
    SequenceWindowOverflow {
        /// L1 origin block number.
        l1_origin: u64,
        /// Sequence window added to the L1 origin.
        sequence_window: u64,
    },
    /// Fetching the finalized L1 block failed.
    #[error("failed to fetch finalized L1 block")]
    FinalizedBlockFetch {
        /// Underlying RPC error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The L1 RPC endpoint did not return a finalized block.
    #[error("finalized L1 block not found")]
    FinalizedBlockMissing,
    /// Fetching a specific L1 block failed.
    #[error("failed to fetch L1 block {block_number}")]
    BlockFetch {
        /// Requested L1 block number.
        block_number: u64,
        /// Underlying RPC error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The requested L1 block was missing from the L1 RPC endpoint.
    #[error("L1 block {block_number} not found")]
    BlockMissing {
        /// Requested L1 block number.
        block_number: u64,
    },
}

/// Computes safe L1 head blocks for proof ranges.
#[derive(Debug)]
pub struct L1HeadCalculator;

impl L1HeadCalculator {
    /// Calculate L1 head block number and hash.
    ///
    /// Returns `(l1_head_block_number, l1_head_hash)`.
    pub async fn calculate_l1_head<L1, OP>(
        l1_provider: &L1,
        base_provider: &OP,
        l2_block_number: u64,
        sequence_window: u64,
    ) -> Result<(u64, B256), L1HeadError>
    where
        L1: Provider,
        OP: Provider<Base>,
    {
        debug!(
            l2_block_number = l2_block_number,
            sequence_window = sequence_window,
            "calculating L1 head"
        );

        let l1_origin = Self::get_l1_origin_num(base_provider, l2_block_number).await?;
        debug!(l1_origin = l1_origin, l2_block_number = l2_block_number, "retrieved L1 origin");

        let desired_l1_head = l1_origin
            .checked_add(sequence_window)
            .ok_or(L1HeadError::SequenceWindowOverflow { l1_origin, sequence_window })?;

        let finalized_block = l1_provider
            .get_block(BlockId::Number(BlockNumberOrTag::Finalized))
            .await
            .map_err(|source| L1HeadError::FinalizedBlockFetch { source: Box::new(source) })?
            .ok_or(L1HeadError::FinalizedBlockMissing)?;

        let finalized_block_num = finalized_block.header.number;
        let l1_head_block_num = desired_l1_head.min(finalized_block_num);

        if l1_head_block_num < desired_l1_head {
            debug!(
                desired = desired_l1_head,
                finalized = finalized_block_num,
                used = l1_head_block_num,
                "capped L1 head to finalized block"
            );
        }

        let l1_head_hash = if l1_head_block_num == finalized_block_num {
            finalized_block.header.hash
        } else {
            Self::get_block_hash(l1_provider, l1_head_block_num).await?
        };

        debug!(
            l1_head_block_num = l1_head_block_num,
            l1_head_hash = %l1_head_hash,
            "calculated L1 head"
        );

        Ok((l1_head_block_num, l1_head_hash))
    }

    /// Calculate L1 head block number and hash from RPC endpoint URLs.
    ///
    /// Returns `(l1_head_block_number, l1_head_hash)`.
    pub async fn calculate_l1_head_from_urls(
        l1_node_url: &str,
        base_consensus_url: &str,
        l2_block_number: u64,
        sequence_window: u64,
    ) -> Result<(u64, B256), L1HeadError> {
        let l1_rpc_url =
            Url::parse(l1_node_url).map_err(|source| L1HeadError::InvalidL1RpcUrl { source })?;
        let l1_provider = ProviderBuilder::new().connect_http(l1_rpc_url);
        let base_rpc_url = Url::parse(base_consensus_url)
            .map_err(|source| L1HeadError::InvalidBaseConsensusUrl { source })?;
        let base_provider =
            ProviderBuilder::<Identity, Identity, Base>::default().connect_http(base_rpc_url);

        Self::calculate_l1_head(&l1_provider, &base_provider, l2_block_number, sequence_window)
            .await
    }

    /// Get L1 origin block number from `optimism_outputAtBlock`.
    pub async fn get_l1_origin_num<OP>(
        base_provider: &OP,
        l2_block_number: u64,
    ) -> Result<u64, L1HeadError>
    where
        OP: Provider<Base>,
    {
        let response = base_provider
            .optimism_output_at_block(BlockNumberOrTag::Number(l2_block_number))
            .await
            .map_err(|source| L1HeadError::L1OriginFetch { source: Box::new(source) })?;

        Ok(response.block_ref.l1origin.number)
    }

    /// Get the hash for a specific L1 block number.
    pub async fn get_block_hash<P>(provider: &P, block_number: u64) -> Result<B256, L1HeadError>
    where
        P: Provider,
    {
        let block = provider
            .get_block(BlockId::Number(BlockNumberOrTag::Number(block_number)))
            .await
            .map_err(|source| L1HeadError::BlockFetch { block_number, source: Box::new(source) })?
            .ok_or(L1HeadError::BlockMissing { block_number })?;

        Ok(block.header.hash)
    }
}
