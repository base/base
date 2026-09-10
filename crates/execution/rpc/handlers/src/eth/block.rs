//! Loads and formats Base block RPC response.

use crate::RpcBlockConverter;
use alloy_eips::BlockId;
use base_common_types_chain::BlockHeader as AlloyBlockHeader;
use base_common_types_rpc::{BaseBlockResponse, Header};

use crate::{BaseEthApi, BaseEthApiError};

impl BaseEthApi {
    pub async fn rpc_block_header(
        &self,
        block_id: BlockId,
    ) -> Result<Option<Header>, BaseEthApiError> {
        let Some(block) = self.recovered_block(block_id).await? else { return Ok(None) };
        let timestamp_ms = self.base_time_cache().insert_from_transactions(
            block.hash(),
            block.number(),
            block.timestamp(),
            &block.body().transactions,
        );
        let mut header =
            self.converter().convert_header(block.clone_sealed_header(), block.rlp_length())?;
        header.timestamp_ms = timestamp_ms;
        Ok(Some(header))
    }

    pub async fn rpc_block(
        &self,
        block_id: BlockId,
        full: bool,
    ) -> Result<Option<BaseBlockResponse>, BaseEthApiError> {
        let Some(block) = self.recovered_block(block_id).await? else { return Ok(None) };
        let timestamp_ms = self.base_time_cache().insert_from_transactions(
            block.hash(),
            block.number(),
            block.timestamp(),
            &block.body().transactions,
        );
        let mut block = RpcBlockConverter::clone_into_rpc_block(
            &block,
            full.into(),
            |tx, tx_info| self.converter().fill(tx, tx_info),
            |header, size| self.converter().convert_header(header, size),
        )?;
        block.header.timestamp_ms = timestamp_ms;
        Ok(Some(block))
    }
}
