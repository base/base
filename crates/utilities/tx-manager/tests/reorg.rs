//! Integration coverage for canonical receipt checks across L1 reorgs.

pub mod common;

use std::{sync::Arc, time::Duration};

use alloy_primitives::U256;
use alloy_provider::Provider;
use base_runtime::TokioRuntime;
use base_tx_manager::{ChainSweeper, NoopTxMetrics, PublishedAttempt, TxManager, VersionKind};
use common::{fast_config, mine_block, setup_with_config, value_transfer};

#[tokio::test]
async fn canonical_receipt_disappears_when_a_reorg_removes_its_block() {
    let (manager, provider, _anvil) = setup_with_config(fast_config()).await;
    let snapshot: U256 =
        provider.raw_request("evm_snapshot".into(), ()).await.expect("snapshot should succeed");
    let receipt =
        manager.submit(value_transfer(1)).wait().await.expect("transaction should confirm");
    let confirmed_height = provider.get_block_number().await.expect("block number should load");
    let attempt = PublishedAttempt { kind: VersionKind::Original, hash: receipt.transaction_hash };
    let sweeper = ChainSweeper::new(
        provider.clone(),
        TokioRuntime::new(),
        manager.sender_address(),
        1,
        Duration::from_secs(1),
        Arc::new(NoopTxMetrics),
    );

    assert!(
        sweeper
            .canonical_receipt(attempt, confirmed_height)
            .await
            .expect("canonical lookup should succeed")
            .is_some()
    );

    let reverted: bool =
        provider.raw_request("evm_revert".into(), [snapshot]).await.expect("revert should succeed");
    assert!(reverted);
    mine_block(&provider).await;
    let confirmed_height = provider.get_block_number().await.expect("block number should load");

    assert!(
        sweeper
            .canonical_receipt(attempt, confirmed_height)
            .await
            .expect("canonical lookup should succeed")
            .is_none()
    );
}
