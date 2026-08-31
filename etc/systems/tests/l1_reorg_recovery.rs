//! System test for sequencer recovery from an L1 outage followed by a deep reorg.

use std::time::Duration;

use alloy_eips::{BlockNumberOrTag, NumHash};
use alloy_network::Ethereum;
use alloy_primitives::B256;
use alloy_provider::{Provider, RootProvider};
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_protocol::{L2BlockInfo, SyncStatus};
use base_system_tests::{
    L1ReorgDriver, L1RpcProxy, SystemTestProviderExt, SystemTestRpcClient, SystemTestStackBuilder,
};
use eyre::{OptionExt, Result, WrapErr, ensure};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const L1_BLOCK_TIME: u64 = 4;
const REORG_DEPTH: u64 = 6;
const OUTAGE_BLOCKS: u64 = 10;
const RECOVERY_BLOCKS: u64 = 5;
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const ORIGIN_TIMEOUT: Duration = Duration::from_secs(90);
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(180);

#[tokio::test]
async fn sequencer_recovers_from_l1_outage_and_deep_reorg() -> Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_slot_duration(L1_BLOCK_TIME)
        .with_l1_fault_injection()
        .build()
        .await?;
    let urls = system.urls().await?;
    let rpc = SystemTestRpcClient::new(
        &urls.l1_rpc,
        &urls.l2_builder_rpc,
        &urls.l2_client_rpc,
        &urls.l2_builder_consensus_rpc,
        &urls.l2_client_consensus_rpc,
    )?;
    let l1_provider = system.l1_provider().await?;
    let builder = system.l2_builder_provider()?;
    let client = system.l2_client_provider()?;
    let rollup_config: RollupConfig =
        serde_json::from_str(&system.l2_deployment().read_rollup_config()?)?;

    wait_for_reorgable_origin(&rpc, &l1_provider).await?;
    let proxy = system.l1_rpc_proxy().ok_or_eyre("L1 fault proxy was not configured")?;
    let l1_head_before_outage = l1_provider.get_block_number().await?;
    proxy.disable();
    wait_for_rejected_request(proxy).await?;

    let settling_height = builder.get_block_number().await? + 2;
    builder.wait_for_block(settling_height, ORIGIN_TIMEOUT).await?;
    let outage_start = builder.get_block_number().await?;
    let cached_origin = rpc.l2_builder_sync_status().await?.unsafe_l2.l1_origin;
    let old_l2_tip = builder.wait_for_block(outage_start + OUTAGE_BLOCKS, ORIGIN_TIMEOUT).await?;
    ensure!(
        l1_provider.get_block_number().await? > l1_head_before_outage,
        "L1 did not advance while its RPC was unavailable to L2 services"
    );
    ensure!(
        rpc.l2_builder_sync_status().await?.unsafe_l2.l1_origin == cached_origin,
        "sequencer did not continue on one cached L1 origin throughout the outage"
    );
    client.wait_for_block(old_l2_tip, ORIGIN_TIMEOUT).await?;
    client.wait_for_convergence(&builder, old_l2_tip, ORIGIN_TIMEOUT).await?;
    let mut outage_hashes = Vec::new();
    for height in (outage_start + 1)..=old_l2_tip {
        let block = l2_block_info(&builder, height, &rollup_config).await?;
        ensure!(
            block.l1_origin == cached_origin,
            "L2 block {height} did not use the cached L1 origin during the outage"
        );
        ensure!(
            client.block_hash_at(height).await? == Some(block.block_info.hash),
            "validator did not hold the outage chain at L2 block {height}"
        );
        outage_hashes.push((height, block.block_info.hash));
    }

    system.l1_stack().stop_consensus().await?;
    let old_l1_head = l1_provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .ok_or_eyre("L1 latest block is missing")?;
    let old_origin_hash = l1_block_hash(&l1_provider, cached_origin.number).await?;
    ensure!(
        old_origin_hash == cached_origin.hash,
        "the sequencer's cached origin was not canonical before fault injection"
    );
    let common_ancestor_number = cached_origin
        .number
        .checked_sub(REORG_DEPTH)
        .ok_or_eyre("cached origin is too early for the configured reorg depth")?;
    let common_ancestor_hash = l1_block_hash(&l1_provider, common_ancestor_number).await?;

    system.l1_stack().reth().unwind_to(common_ancestor_number).await?;
    let reorg = L1ReorgDriver::new(
        system.l1_rpc_url().await?,
        system.l1_stack().engine_url().await?,
        &system.l1_genesis().read_jwt_secret()?,
    )?;
    let replacement = reorg
        .build_replacement(
            old_l1_head.header.number + 2,
            old_l1_head.header.timestamp + L1_BLOCK_TIME,
            L1_BLOCK_TIME,
        )
        .await?;
    ensure!(
        replacement.common_ancestor == common_ancestor_hash,
        "replacement L1 branch started from an unexpected ancestor"
    );
    ensure!(
        replacement.replacement_hashes.len()
            == usize::try_from(old_l1_head.header.number + 2 - common_ancestor_number)?,
        "replacement L1 branch has an unexpected length"
    );
    let replacement_origin_hash = l1_block_hash(&l1_provider, cached_origin.number).await?;
    ensure!(
        replacement_origin_hash != old_origin_hash,
        "the replacement L1 branch did not orphan the cached sequencer origin"
    );

    proxy.enable();
    for (height, old_hash) in &outage_hashes {
        wait_for_changed_block(&builder, *height, *old_hash).await?;
        let rebuilt = l2_block_info(&builder, *height, &rollup_config).await?;
        ensure!(
            l1_block_hash(&l1_provider, rebuilt.l1_origin.number).await? == rebuilt.l1_origin.hash,
            "rebuilt L2 block {height} retained a noncanonical L1 origin"
        );
    }
    let rebuilt_tip =
        builder.wait_for_block(old_l2_tip + RECOVERY_BLOCKS, RECOVERY_TIMEOUT).await?;
    wait_for_replacement_origin(
        &rpc,
        common_ancestor_number,
        cached_origin.number,
        &replacement.replacement_hashes,
    )
    .await?;

    let adoption_tip = builder.get_block_number().await?.max(rebuilt_tip);
    builder.wait_for_block(adoption_tip + RECOVERY_BLOCKS, RECOVERY_TIMEOUT).await?;

    Ok(())
}

async fn wait_for_rejected_request(proxy: &L1RpcProxy) -> Result<()> {
    timeout(ORIGIN_TIMEOUT, async {
        while proxy.rejected_requests() == 0 {
            sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("L1 fault proxy did not reject a request after the outage began")
}

async fn wait_for_reorgable_origin(
    rpc: &SystemTestRpcClient,
    l1_provider: &RootProvider<Ethereum>,
) -> Result<SyncStatus> {
    timeout(ORIGIN_TIMEOUT, async {
        loop {
            let status = rpc.l2_builder_sync_status().await?;
            let Some(safe) = l1_provider.get_block_by_number(BlockNumberOrTag::Safe).await? else {
                sleep(POLL_INTERVAL).await;
                continue;
            };
            let protected_origin = safe.header.number.max(status.finalized_l2.l1_origin.number);
            if status.unsafe_l2.l1_origin.number > protected_origin + REORG_DEPTH {
                return Ok::<_, eyre::Error>(status);
            }
            sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("sequencer did not reach an L1 origin deep enough to reorg")?
}

async fn wait_for_changed_block(
    provider: &RootProvider<Base>,
    height: u64,
    old_hash: B256,
) -> Result<B256> {
    timeout(RECOVERY_TIMEOUT, async {
        loop {
            if let Some(hash) = provider.block_hash_at(height).await?
                && hash != old_hash
            {
                return Ok::<_, eyre::Error>(hash);
            }
            sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("sequencer did not automatically replace blocks built on the orphaned L1 origin")?
}

async fn wait_for_replacement_origin(
    rpc: &SystemTestRpcClient,
    common_ancestor_number: u64,
    minimum_origin_number: u64,
    replacement_hashes: &[B256],
) -> Result<()> {
    timeout(RECOVERY_TIMEOUT, async {
        loop {
            let origin = rpc.l2_builder_sync_status().await?.unsafe_l2.l1_origin;
            if origin.number >= minimum_origin_number
                && replacement_contains_origin(common_ancestor_number, replacement_hashes, origin)
            {
                return Ok::<_, eyre::Error>(());
            }
            sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("sequencer did not adopt an origin from the replacement L1 branch")?
}

fn replacement_contains_origin(
    common_ancestor_number: u64,
    replacement_hashes: &[B256],
    origin: NumHash,
) -> bool {
    origin
        .number
        .checked_sub(common_ancestor_number + 1)
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| replacement_hashes.get(index))
        == Some(&origin.hash)
}

async fn l2_block_info(
    provider: &RootProvider<Base>,
    number: u64,
    rollup_config: &RollupConfig,
) -> Result<L2BlockInfo> {
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .full()
        .await?
        .ok_or_eyre("L2 block is missing at the requested height")?
        .map_header(|header| header.into_inner())
        .into_consensus()
        .map_transactions(|transaction| transaction.inner.inner);
    L2BlockInfo::from_block_and_genesis(&block, &rollup_config.genesis)
        .wrap_err("Failed to decode L1 origin from L2 block")
}

async fn l1_block_hash(provider: &RootProvider<Ethereum>, number: u64) -> Result<B256> {
    provider
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .await?
        .map(|block| block.header.hash)
        .ok_or_eyre("L1 block is missing at the requested height")
}
