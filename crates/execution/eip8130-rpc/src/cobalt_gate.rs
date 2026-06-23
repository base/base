//! Cobalt fork-activation gate for EIP-8130 RPC reads.

use alloy_consensus::BlockHeader;
use alloy_eips::{BlockId, BlockNumberOrTag};
use base_common_chains::Upgrades;
use base_common_network::Base;
use jsonrpsee_types::{
    ErrorObjectOwned,
    error::{INTERNAL_ERROR_CODE, INVALID_PARAMS_CODE},
};
use reth_chainspec::ChainSpecProvider;
use reth_rpc_eth_api::{RpcNodeCore, helpers::FullEthApi};
use reth_storage_api::BlockReaderIdExt;
use tracing::warn;

/// Rejects EIP-8130 RPC reads issued before the Cobalt hard fork has
/// activated at the requested block.
///
/// Mirrors the txpool's Cobalt gate (which rejects EIP-8130 transactions
/// with `TxTypeNotSupported` pre-activation) on the read side. Callers
/// invoke this only on paths that actually consult the precompile (i.e.
/// `nonce_key != 0`); requests with no `nonce_key` or `Some(0)` are
/// indistinguishable from a legacy `eth_getTransactionCount` and bypass
/// the gate to keep the hot path free of a sync header resolution.
///
/// The block-of-query timestamp drives the gate, so historical queries
/// against pre-Cobalt blocks are gated even when Cobalt is currently active.
#[derive(Debug)]
pub struct Eip8130CobaltGate;

impl Eip8130CobaltGate {
    /// Errors with `INVALID_PARAMS` if Cobalt is not active at `block_id`'s
    /// timestamp. Resolves `Pending` and `Latest` block ids to the head
    /// block's timestamp.
    pub fn check<Eth>(eth_api: &Eth, block_id: BlockId) -> Result<(), ErrorObjectOwned>
    where
        Eth: FullEthApi<NetworkTypes = Base>,
        <Eth as RpcNodeCore>::Provider: ChainSpecProvider + BlockReaderIdExt,
        <<Eth as RpcNodeCore>::Provider as ChainSpecProvider>::ChainSpec: Upgrades,
    {
        let provider = eth_api.provider();
        let timestamp = Self::resolve_timestamp(provider, block_id)?;
        if !provider.chain_spec().is_cobalt_active_at_timestamp(timestamp) {
            return Err(ErrorObjectOwned::owned(
                INVALID_PARAMS_CODE,
                "EIP-8130 RPC features are not active before the Cobalt hard fork; the `nonce_key` parameter is not supported at this block",
                None::<()>,
            ));
        }
        Ok(())
    }

    /// Resolves a `BlockId` to a block timestamp via the provider's
    /// header lookup, falling back to the latest sealed header for
    /// `Pending`.
    fn resolve_timestamp<P>(provider: &P, block_id: BlockId) -> Result<u64, ErrorObjectOwned>
    where
        P: BlockReaderIdExt,
    {
        let header = match block_id {
            BlockId::Number(BlockNumberOrTag::Pending) => {
                provider.sealed_header_by_number_or_tag(BlockNumberOrTag::Latest)
            }
            BlockId::Number(tag) => provider.sealed_header_by_number_or_tag(tag),
            BlockId::Hash(hash) => provider.sealed_header_by_hash(hash.block_hash),
        }
        .map_err(|err| {
            warn!(
                error = %err,
                block_id = ?block_id,
                "cobalt gate: failed to resolve block header"
            );
            ErrorObjectOwned::owned(INTERNAL_ERROR_CODE, "failed to resolve block", None::<()>)
        })?
        .ok_or_else(|| {
            ErrorObjectOwned::owned(
                INVALID_PARAMS_CODE,
                format!("block not found: {block_id}"),
                None::<()>,
            )
        })?;
        Ok(header.timestamp())
    }
}
