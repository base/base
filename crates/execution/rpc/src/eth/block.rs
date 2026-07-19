//! Loads and formats Base block RPC response.

use alloy_eips::BlockId;
use base_common_consensus::BaseTransaction;
use base_common_rpc_types::BaseHeaderResponse;
use base_protocol::BaseTimeUpdateTx;
use reth_node_api::BlockBody;
use reth_primitives_traits::{AlloyBlockHeader, NodePrimitives};
use reth_rpc_eth_api::{
    EthApiTypes, FromEvmError, FullEthApiTypes, RpcBlock, RpcConvert, RpcHeader, RpcTypes,
    helpers::{EthBlocks, LoadBlock},
};

use crate::{BaseEthApi, BaseEthApiError, eth::RpcNodeCore};

trait TimestampMsHeader {
    fn set_timestamp_ms(&mut self, timestamp_ms: Option<u64>);
}

impl TimestampMsHeader for BaseHeaderResponse {
    fn set_timestamp_ms(&mut self, timestamp_ms: Option<u64>) {
        self.timestamp_ms = timestamp_ms;
    }
}

impl<N: RpcNodeCore, Rpc: RpcConvert> BaseEthApi<N, Rpc> {
    fn extract_timestamp_ms<B: BlockBody>(
        body: &B,
        block_number: u64,
        timestamp: u64,
    ) -> Option<u64>
    where
        B::Transaction: BaseTransaction,
    {
        let millis = BaseTimeUpdateTx::extract_from_transactions(body.transactions(), block_number)
            .ok()?
            .timestamp_millis_part();
        Some(timestamp.wrapping_mul(1_000).wrapping_add(u64::from(millis)))
    }
}

impl<N, Rpc> EthBlocks for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError>,
    <Self::Primitives as NodePrimitives>::SignedTx: BaseTransaction,
    <Self::NetworkTypes as RpcTypes>::Header: TimestampMsHeader,
{
    async fn rpc_block_header(
        &self,
        block_id: BlockId,
    ) -> Result<Option<RpcHeader<Self::NetworkTypes>>, Self::Error>
    where
        Self: FullEthApiTypes,
    {
        let Some(block) = self.recovered_block(block_id).await? else { return Ok(None) };
        let timestamp_ms =
            Self::extract_timestamp_ms(block.body(), block.number(), block.timestamp());
        let mut header =
            self.converter().convert_header(block.clone_sealed_header(), block.rlp_length())?;
        header.set_timestamp_ms(timestamp_ms);
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
        let timestamp_ms =
            Self::extract_timestamp_ms(block.body(), block.number(), block.timestamp());
        let mut block = block.clone_into_rpc_block(
            full.into(),
            |tx, tx_info| self.converter().fill(tx, tx_info),
            |header, size| self.converter().convert_header(header, size),
        )?;
        block.header.set_timestamp_ms(timestamp_ms);
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
