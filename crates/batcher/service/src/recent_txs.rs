//! Startup scan for recent L1 transactions from the batcher account.

use alloy_primitives::Address;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::BlockNumberOrTag;
use tracing::{info, warn};

/// Maximum depth allowed for the recent-transaction startup scan.
pub const MAX_CHECK_RECENT_TXS_DEPTH: u64 = 128;

/// Returns an L1 synchronization target based on recent batcher transactions.
///
/// The result never changes the L2 backfill cursor.
pub async fn recent_tx_sync_target(
    l1_provider: &RootProvider,
    batcher_address: Address,
    depth: u64,
) -> eyre::Result<u64> {
    let current_l1 = l1_provider
        .get_block_number()
        .await
        .map_err(|e| eyre::eyre!("failed to fetch L1 head for recent tx scan: {e}"))?;
    let oldest_l1 = current_l1.saturating_sub(depth);

    let current_nonce = nonce_at(l1_provider, batcher_address, current_l1).await?;
    let oldest_nonce = nonce_at(l1_provider, batcher_address, oldest_l1).await?;

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
        let nonce = nonce_at(l1_provider, batcher_address, mid).await?;
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

async fn nonce_at(
    l1_provider: &RootProvider,
    batcher_address: Address,
    block_number: u64,
) -> eyre::Result<u64> {
    l1_provider
        .get_transaction_count(batcher_address)
        .block_id(BlockNumberOrTag::Number(block_number).into())
        .await
        .map_err(|e| eyre::eyre!("failed to fetch batcher nonce at L1 block {block_number}: {e}"))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use alloy_provider::RootProvider;
    use httpmock::prelude::*;

    use super::recent_tx_sync_target;

    async fn mock_rpc(server: &MockServer, request: String, id: u64, result: &str) {
        let response = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":"{result}"}}"#);
        server
            .mock_async(move |when, then| {
                when.method(POST).path("/").json_body_includes(request.as_str());
                then.status(200).header("content-type", "application/json").body(response);
            })
            .await;
    }

    #[tokio::test]
    async fn finds_block_where_current_nonce_was_reached() {
        let server = MockServer::start_async().await;
        let address = Address::ZERO;
        let address = format!("{address:#x}");

        mock_rpc(&server, r#"{"method":"eth_blockNumber"}"#.into(), 0, "0xa").await;
        for (id, block, nonce) in
            [(1, "0xa", "0x3"), (2, "0x5", "0x1"), (3, "0x8", "0x2"), (4, "0x9", "0x3")]
        {
            mock_rpc(
                &server,
                format!(
                    r#"{{"method":"eth_getTransactionCount","params":["{address}","{block}"]}}"#
                ),
                id,
                nonce,
            )
            .await;
        }

        let provider = RootProvider::new_http(server.url("/").parse().unwrap());
        assert_eq!(recent_tx_sync_target(&provider, Address::ZERO, 5).await.unwrap(), 9);
    }
}
