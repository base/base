//! System tests for follow-mode consensus.

use std::time::Duration;

use alloy_consensus::proofs;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use alloy_provider::{Provider, RootProvider};
use base_common_network::Base;
use base_system_tests::SystemTestStackBuilder;
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const ISTHMUS_ACTIVATION_BLOCK: u64 = 0;
const FOLLOW_SYNC_TIMEOUT: Duration = Duration::from_secs(60);
const ISTHMUS_BLOCK_TIMEOUT: Duration = Duration::from_secs(30);
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);
const MIN_POST_ISTHMUS_BLOCK: u64 = 1;

#[tokio::test]
async fn follow_mode_missing_requests_hash_reproduces_withdrawals_root_bug() -> Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();

    let system = SystemTestStackBuilder::new()
        .with_isthmus_activation_block(ISTHMUS_ACTIVATION_BLOCK)
        .with_follow_source_missing_requests_hash()
        .build()
        .await?;

    let builder_provider = system.l2_builder_provider()?;
    let client_provider = system.l2_client_provider()?;

    let (target_block, builder_hash, builder_withdrawals_root) =
        wait_for_builder_isthmus_block(&builder_provider).await?;

    let result = async {
        let (client_hash, client_withdrawals_root) =
            wait_for_client_block(&client_provider, target_block).await?;

        eyre::ensure!(
            client_hash == builder_hash,
            "follow-mode client imported a different block at {target_block}"
        );
        eyre::ensure!(
            client_withdrawals_root == builder_withdrawals_root,
            "follow-mode client did not preserve the source Isthmus withdrawals root at block {target_block}"
        );

        Ok::<_, eyre::Error>(())
    }
    .await;

    // This is a pre-fix regression reproduction. When follow mode preserves the source
    // withdrawals root without `requestsHash`, invert this assertion to `result.is_ok()`.
    assert!(
        result.is_err(),
        "expected stripped requestsHash to reproduce the pre-fix withdrawals_root bug"
    );

    Ok(())
}

async fn wait_for_builder_isthmus_block(
    provider: &RootProvider<Base>,
) -> Result<(u64, B256, B256)> {
    let empty_withdrawals_root = proofs::calculate_withdrawals_root(&[]);

    timeout(ISTHMUS_BLOCK_TIMEOUT, async {
        let mut next_block = MIN_POST_ISTHMUS_BLOCK;
        loop {
            let head = provider.get_block_number().await?;
            while next_block <= head {
                let block = provider
                    .get_block_by_number(BlockNumberOrTag::Number(next_block))
                    .await
                    .wrap_err_with(|| format!("failed to fetch builder block {next_block}"))?
                    .ok_or_else(|| eyre::eyre!("builder block {next_block} was unavailable"))?;

                let withdrawals_root = block.header.withdrawals_root.ok_or_else(|| {
                    eyre::eyre!("builder block {next_block} did not include withdrawals_root")
                })?;
                if withdrawals_root != empty_withdrawals_root {
                    return Ok::<_, eyre::Error>((
                        block.header.number,
                        block.header.hash,
                        withdrawals_root,
                    ));
                }

                next_block += 1;
            }

            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("builder did not produce a post-Isthmus block with a non-empty withdrawals root")?
}

async fn wait_for_client_block(
    provider: &RootProvider<Base>,
    target_block: u64,
) -> Result<(B256, B256)> {
    timeout(FOLLOW_SYNC_TIMEOUT, async {
        loop {
            if let Some(block) =
                provider.get_block_by_number(BlockNumberOrTag::Number(target_block)).await?
            {
                let withdrawals_root = block.header.withdrawals_root.ok_or_else(|| {
                    eyre::eyre!("client block {target_block} did not include withdrawals_root")
                })?;
                return Ok::<_, eyre::Error>((block.header.hash, withdrawals_root));
            }

            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err_with(|| {
        format!("follow-mode client did not import post-Isthmus block {target_block}")
    })?
}
