//! Helpers for `eth_blockAccessList` RPC method.
use alloy_eip7928::{BlockAccessList, bal::DecodedBal};
use alloy_primitives::Bytes;
use base_common_consensus::BlockHeader;
use base_common_rpc_types::BlockId;
use base_evm_handler::database::State;
use base_execution_evm::{BlockExecutor, Evm};
use reth_rpc_eth_types::{BaseEthApiError, EthApiError};
use reth_storage_api::StateProviderFactory;

use crate::BaseEthApi;

/// Helper trait for `eth_blockAccessList` RPC method.
impl BaseEthApi {
    /// Retrieves the block access list for a block identified by its hash.
    pub fn get_block_access_list(
        &self,
        block_id: BlockId,
    ) -> impl Future<Output = Result<Option<BlockAccessList>, BaseEthApiError>> + Send {
        async move {
            let Some(block) = self.recovered_block(block_id).await? else {
                return Ok(None);
            };

            if let Some(cached_bal) =
                self.cache().get_bal(block.hash()).await.map_err(BaseEthApiError::from_eth_err)?
            {
                let (bal, _) = DecodedBal::from_rlp_bytes(cached_bal.as_raw().clone())
                    .map_err(|error| reth_rpc_eth_types::EthApiError::Internal(error.into()))
                    .map_err(BaseEthApiError::from_eth_err)?
                    .split();
                return Ok(Some(Vec::from(bal)));
            }

            self.spawn_blocking_io(move |eth_api| {
                let state = eth_api
                    .provider()
                    .state_by_block_id(block.parent_hash().into())
                    .map_err(BaseEthApiError::from_eth_err)?;

                let mut db = State::builder().with_database(state).with_bal_builder().build();

                let block_txs = block.transactions_recovered();
                let mut executor = eth_api
                    .evm_config()
                    .executor_for_block(&mut db, block.sealed_block())
                    .map_err(|error| reth_rpc_eth_types::EthApiError::Internal(error.into()))
                    .map_err(BaseEthApiError::from_eth_err)?;

                executor.apply_pre_execution_changes().map_err(BaseEthApiError::from_eth_err)?;
                executor.evm_mut().db_mut().bump_bal_index();

                // replay all transactions prior to the targeted transaction
                for block_tx in block_txs {
                    executor
                        .execute_transaction(block_tx)
                        .map_err(BaseEthApiError::from_eth_err)?;
                    executor.evm_mut().db_mut().bump_bal_index();
                }

                executor
                    .apply_post_execution_changes()
                    .map_err(|err| EthApiError::Internal(err.into()))?;

                let bal = db.take_built_alloy_bal();
                Ok(bal)
            })
            .await
        }
    }

    /// Retrieves the raw RLP-encoded block access list for a block.
    pub fn get_raw_block_access_list(
        &self,
        block_id: BlockId,
    ) -> impl Future<Output = Result<Option<Bytes>, BaseEthApiError>> + Send {
        async move {
            let block = self
                .recovered_block(block_id)
                .await?
                .ok_or_else(|| EthApiError::HeaderNotFound(block_id))?;

            if let Some(cached_bal) =
                self.cache().get_bal(block.hash()).await.map_err(BaseEthApiError::from_eth_err)?
            {
                return Ok(Some(cached_bal.as_raw().clone()));
            }

            Ok(self.get_block_access_list(block_id).await?.map(|bal| alloy_rlp::encode(bal).into()))
        }
    }
}
