use alloy_consensus::private::alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;
use base_common_evm::L1BlockInfo;
use base_common_network::Base;
use base_execution_evm::extract_l1_info_from_tx;
use jsonrpsee::core::RpcResult;
use reth_rpc_eth_types::{EthApiError, SignError};
use tokio::time::Instant;
use tracing::warn;

use crate::metrics::Metrics;

/// Account info for a given address.
#[derive(Debug)]
pub struct AccountInfo {
    /// Account balance in wei.
    pub balance: U256,
    /// Account transaction nonce.
    pub nonce: u64,
    /// Hash of the account's code.
    pub code_hash: B256,
}

/// Interface for fetching account info for a given address.
#[async_trait]
pub trait AccountInfoLookup: Send + Sync {
    /// Fetches account info for the given address.
    async fn fetch_account_info(&self, address: Address) -> RpcResult<AccountInfo>;
}

/// Implementation of the `AccountInfoLookup` trait for the `RootProvider`
#[async_trait]
impl AccountInfoLookup for RootProvider<Base> {
    async fn fetch_account_info(&self, address: Address) -> RpcResult<AccountInfo> {
        let start = Instant::now();
        let account = self
            .get_account(address)
            .await
            .map_err(|_| EthApiError::Signing(SignError::NoAccount))?;
        Metrics::rpc_latency("eth_getAccount").record(start.elapsed().as_secs_f64());

        Ok(AccountInfo {
            balance: account.balance,
            nonce: account.nonce,
            code_hash: account.code_hash,
        })
    }
}

/// Interface for fetching L1 block info for a given block number.
#[async_trait]
pub trait L1BlockInfoLookup: Send + Sync {
    /// Fetches the L1 block info from the latest L2 block.
    async fn fetch_l1_block_info(&self) -> RpcResult<L1BlockInfo>;
}

/// Implementation of the `L1BlockInfoLookup` trait for the `RootProvider`
#[async_trait]
impl L1BlockInfoLookup for RootProvider<Base> {
    async fn fetch_l1_block_info(&self) -> RpcResult<L1BlockInfo> {
        let start = Instant::now();
        let block = self
            .get_block(BlockId::Number(BlockNumberOrTag::Latest))
            .full()
            .await
            .map_err(|e| {
                warn!(message = "failed to fetch latest block", err = %e);
                EthApiError::InternalEthError.into_rpc_err()
            })?
            .ok_or_else(|| {
                warn!(message = "empty latest block returned");
                EthApiError::InternalEthError.into_rpc_err()
            })?;
        Metrics::rpc_latency("eth_getBlockByNumber").record(start.elapsed().as_secs_f64());

        let txs = block.transactions;
        let first_tx = txs.first_transaction().ok_or_else(|| {
            warn!(message = "block contains no transactions");
            EthApiError::InternalEthError.into_rpc_err()
        })?;

        Ok(extract_l1_info_from_tx(&first_tx.clone()).map_err(|e| {
            warn!(message = "failed to extract l1_info from tx", err = %e);
            EthApiError::InternalEthError.into_rpc_err()
        })?)
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip2718::Decodable2718;
    use alloy_primitives::Bytes;
    use base_common_consensus::BaseTxEnvelope;

    #[tokio::test]
    async fn test_decode_tx_rejects_empty_bytes() {
        // Test that empty bytes fail to decode
        let empty_bytes = Bytes::new();
        let result = BaseTxEnvelope::decode_2718(&mut empty_bytes.as_ref());
        assert!(result.is_err(), "Empty bytes should fail decoding");
    }

    #[tokio::test]
    async fn test_decode_tx_rejects_invalid_bytes() {
        // Test that malformed bytes fail to decode
        let invalid_bytes = Bytes::from(vec![0x01, 0x02, 0x03]);
        let result = BaseTxEnvelope::decode_2718(&mut invalid_bytes.as_ref());
        assert!(result.is_err(), "Invalid bytes should fail decoding");
    }
}
