//! Alloy-backed implementation of [`BlockPayloadProvider`] for the req/resp sync server.

use std::{fmt, sync::Arc};

use alloy_consensus::BlockHeader;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2};
use async_trait::async_trait;
use base_alloy_network::Base;
use base_consensus_genesis::RollupConfig;
use base_consensus_gossip::BlockPayloadProvider;
use ssz::Encode;

/// Maximum uncompressed payload size accepted by the req/resp protocol.
///
/// Payloads whose uncompressed SSZ encoding exceeds this limit are not served to avoid sending
/// excessively large responses.
const MAX_UNCOMPRESSED_SIZE: usize = 10 * 1024 * 1024; // 10 MB

/// Alloy-backed [`BlockPayloadProvider`] that fetches L2 blocks over JSON-RPC and encodes them
/// per the OP Stack payload-by-number req/resp protocol.
///
/// Version semantics:
/// - Version `0`: pre-Ecotone — `snappy(ssz(ExecutionPayloadV1))`
/// - Version `1`: Ecotone — `snappy(ssz(ExecutionPayloadV2) ++ parent_beacon_block_root[32])`
/// - Isthmus+: not served (returns `None`)
#[derive(Clone)]
pub struct AlloyBlockPayloadProvider {
    provider: RootProvider<Base>,
    rollup_config: Arc<RollupConfig>,
}

impl fmt::Debug for AlloyBlockPayloadProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlloyBlockPayloadProvider")
            .field("rollup_config", &self.rollup_config)
            .finish_non_exhaustive()
    }
}

impl AlloyBlockPayloadProvider {
    /// Creates a new [`AlloyBlockPayloadProvider`].
    pub const fn new(provider: RootProvider<Base>, rollup_config: Arc<RollupConfig>) -> Self {
        Self { provider, rollup_config }
    }
}

#[async_trait]
impl BlockPayloadProvider for AlloyBlockPayloadProvider {
    async fn get_payload(&self, block_number: u64) -> Option<(u32, Vec<u8>)> {
        let rpc_block =
            self.provider.get_block_by_number(block_number.into()).full().await.ok().flatten()?;

        let timestamp = rpc_block.header.timestamp();
        let block_hash = rpc_block.header.hash;
        let parent_beacon_block_root = rpc_block.header.parent_beacon_block_root;

        // Isthmus+ is out of scope for this implementation.
        if self.rollup_config.is_isthmus_active(timestamp) {
            return None;
        }

        // Pre-Canyon blocks are not supported on Base.
        if !self.rollup_config.is_canyon_active(timestamp) {
            return None;
        }

        // Convert RPC block to a consensus block for payload construction.
        let consensus_block =
            rpc_block.into_consensus().map_transactions(|t| t.inner.inner.into_inner());

        if self.rollup_config.is_ecotone_active(timestamp) {
            // Version 1: ExecutionPayloadV2 + parent_beacon_block_root appended after SSZ.
            let payload = ExecutionPayloadV2::from_block_unchecked(block_hash, &consensus_block);
            let mut ssz_bytes = payload.as_ssz_bytes();
            let pbr = parent_beacon_block_root.unwrap_or_default();
            ssz_bytes.extend_from_slice(pbr.as_slice());

            if ssz_bytes.len() > MAX_UNCOMPRESSED_SIZE {
                return None;
            }

            let compressed = snap::raw::Encoder::new().compress_vec(&ssz_bytes).ok()?;
            Some((1, compressed))
        } else {
            // Version 0: ExecutionPayloadV1.
            let payload = ExecutionPayloadV1::from_block_unchecked(block_hash, &consensus_block);
            let ssz_bytes = payload.as_ssz_bytes();

            if ssz_bytes.len() > MAX_UNCOMPRESSED_SIZE {
                return None;
            }

            let compressed = snap::raw::Encoder::new().compress_vec(&ssz_bytes).ok()?;
            Some((0, compressed))
        }
    }
}
