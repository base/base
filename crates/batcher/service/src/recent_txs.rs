//! Startup scan for recent L1 transactions from the batcher account.

use alloy_primitives::Address;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::BlockNumberOrTag;
use tracing::{info, warn};

/// Maximum depth allowed for the recent-transaction startup scan.
pub const MAX_CHECK_RECENT_TXS_DEPTH: u64 = 128;

/// Selects the L1 synchronization target from recent batcher transactions.
#[derive(Debug)]
pub struct RecentTxSyncTarget;

impl RecentTxSyncTarget {
    /// Returns an L1 synchronization target based on recent batcher transactions.
    ///
    /// The result never changes the L2 backfill cursor.
    pub async fn find(
        l1_provider: &RootProvider,
        batcher_address: Address,
        depth: u64,
    ) -> eyre::Result<u64> {
        let current_l1 = l1_provider
            .get_block_number()
            .await
            .map_err(|e| eyre::eyre!("failed to fetch L1 head for recent tx scan: {e}"))?;
        let oldest_l1 = current_l1.saturating_sub(depth);
        info!(
            depth = %depth,
            scan_start = %oldest_l1,
            scan_end = %current_l1,
            batcher = %batcher_address,
            "scanning recent L1 blocks for batcher nonce activity"
        );
        let nonce_at = |block_number| async move {
            l1_provider
                .get_transaction_count(batcher_address)
                .block_id(BlockNumberOrTag::Number(block_number).into())
                .await
                .map_err(|e| {
                    eyre::eyre!("failed to fetch batcher nonce at L1 block {block_number}: {e}")
                })
        };

        let current_nonce = nonce_at(current_l1).await?;
        let oldest_nonce = nonce_at(oldest_l1).await?;

        if oldest_nonce > current_nonce {
            warn!(target_l1 = %current_l1, "L1 changed during recent transaction scan");
            return Ok(current_l1);
        }

        if oldest_nonce == current_nonce {
            info!(
                scan_start = %oldest_l1,
                scan_end = %current_l1,
                "no recent batcher transaction found on L1"
            );
            return Ok(oldest_l1);
        }

        let mut low = oldest_l1.saturating_add(1);
        let mut high = current_l1;
        while low < high {
            let mid = low + (high - low) / 2;
            let nonce = nonce_at(mid).await?;
            if nonce > current_nonce {
                warn!(target_l1 = %current_l1, "L1 changed during recent transaction scan");
                return Ok(current_l1);
            }
            if nonce == current_nonce {
                high = mid;
            } else {
                low = mid.saturating_add(1);
            }
        }

        info!(l1_block = %low, "found latest batcher transaction in recent L1 window");
        Ok(low)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use alloy_provider::RootProvider;
    use httpmock::prelude::*;

    use super::RecentTxSyncTarget;

    async fn mock_rpc(server: &MockServer, request: String, id: u64, result: &str) {
        let response = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":"{result}"}}"#);
        server
            .mock_async(move |when, then| {
                when.method(POST).path("/").json_body_includes(request.as_str());
                then.status(200).header("content-type", "application/json").body(response);
            })
            .await;
    }

    fn nonce_request(address: &str, block: &str) -> String {
        format!(r#"{{"method":"eth_getTransactionCount","params":["{address}","{block}"]}}"#)
    }

    #[tokio::test]
    async fn finds_block_where_current_nonce_was_reached() {
        let server = MockServer::start_async().await;
        let address = format!("{:#x}", Address::ZERO);

        mock_rpc(&server, r#"{"method":"eth_blockNumber"}"#.into(), 0, "0xa").await;
        for (id, block, nonce) in
            [(1, "0xa", "0x3"), (2, "0x5", "0x1"), (3, "0x8", "0x2"), (4, "0x9", "0x3")]
        {
            mock_rpc(&server, nonce_request(&address, block), id, nonce).await;
        }

        let provider = RootProvider::new_http(server.url("/").parse().unwrap());
        assert_eq!(RecentTxSyncTarget::find(&provider, Address::ZERO, 5).await.unwrap(), 9);
    }

    #[tokio::test]
    async fn uses_oldest_block_when_no_recent_transaction_exists() {
        let server = MockServer::start_async().await;
        let address = format!("{:#x}", Address::ZERO);

        mock_rpc(&server, r#"{"method":"eth_blockNumber"}"#.into(), 0, "0xa").await;
        mock_rpc(&server, nonce_request(&address, "0xa"), 1, "0x3").await;
        mock_rpc(&server, nonce_request(&address, "0x5"), 2, "0x3").await;

        let provider = RootProvider::new_http(server.url("/").parse().unwrap());
        assert_eq!(RecentTxSyncTarget::find(&provider, Address::ZERO, 5).await.unwrap(), 5);
    }

    #[tokio::test]
    async fn uses_head_when_endpoint_nonces_are_inconsistent() {
        let server = MockServer::start_async().await;
        let address = format!("{:#x}", Address::ZERO);

        mock_rpc(&server, r#"{"method":"eth_blockNumber"}"#.into(), 0, "0xa").await;
        mock_rpc(&server, nonce_request(&address, "0xa"), 1, "0x2").await;
        mock_rpc(&server, nonce_request(&address, "0x5"), 2, "0x3").await;

        let provider = RootProvider::new_http(server.url("/").parse().unwrap());
        assert_eq!(RecentTxSyncTarget::find(&provider, Address::ZERO, 5).await.unwrap(), 10);
    }

    #[tokio::test]
    async fn uses_head_when_binary_search_nonce_is_inconsistent() {
        let server = MockServer::start_async().await;
        let address = format!("{:#x}", Address::ZERO);

        mock_rpc(&server, r#"{"method":"eth_blockNumber"}"#.into(), 0, "0xa").await;
        mock_rpc(&server, nonce_request(&address, "0xa"), 1, "0x3").await;
        mock_rpc(&server, nonce_request(&address, "0x5"), 2, "0x1").await;
        mock_rpc(&server, nonce_request(&address, "0x8"), 3, "0x4").await;

        let provider = RootProvider::new_http(server.url("/").parse().unwrap());
        assert_eq!(RecentTxSyncTarget::find(&provider, Address::ZERO, 5).await.unwrap(), 10);
    }
}
