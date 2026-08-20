//! Loads and formats Base block RPC response.

use alloy_eips::BlockId;
use base_common_consensus::BaseTransaction;
use base_common_rpc_types::BaseHeaderResponse;
use reth_node_api::BlockBody;
use reth_primitives_traits::{AlloyBlockHeader, NodePrimitives};
use reth_rpc_eth_api::{
    EthApiTypes, FromEvmError, FullEthApiTypes, RpcBlock, RpcConvert, RpcHeader, RpcTypes,
    helpers::{EthBlocks, LoadBlock},
};

use crate::{BaseEthApi, BaseEthApiError, eth::RpcNodeCore};

impl<N, Rpc> EthBlocks for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError>,
    <Self as EthApiTypes>::NetworkTypes: RpcTypes<Header = BaseHeaderResponse>,
    <Self::Primitives as NodePrimitives>::SignedTx: BaseTransaction,
{
    async fn rpc_block_header(
        &self,
        block_id: BlockId,
    ) -> Result<Option<RpcHeader<Self::NetworkTypes>>, Self::Error>
    where
        Self: FullEthApiTypes,
    {
        let Some(block) = self.recovered_block(block_id).await? else { return Ok(None) };
        let timestamp_ms = self.base_time_cache().insert_from_transactions(
            block.hash(),
            block.number(),
            block.timestamp(),
            block.body().transactions(),
        );
        let mut header =
            self.converter().convert_header(block.clone_sealed_header(), block.rlp_length())?;
        header.timestamp_ms = timestamp_ms;
        Ok(Some(header))
    }

    async fn rpc_block(
        &self,
        block_id: BlockId,
        full: bool,
    ) -> Result<Option<RpcBlock<Self::NetworkTypes>>, Self::Error>
    where
        Self: FullEthApiTypes,
    {
        let Some(block) = self.recovered_block(block_id).await? else { return Ok(None) };
        let timestamp_ms = self.base_time_cache().insert_from_transactions(
            block.hash(),
            block.number(),
            block.timestamp(),
            block.body().transactions(),
        );
        let mut block = block.clone_into_rpc_block(
            full.into(),
            |tx, tx_info| self.converter().fill(tx, tx_info),
            |header, size| self.converter().convert_header(header, size),
        )?;
        block.header.timestamp_ms = timestamp_ms;
        Ok(Some(block))
    }
}

impl<N, Rpc> LoadBlock for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError>,
{
}
