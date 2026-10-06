//! Full-stack coverage for post-Isthmus gossip subscriptions and unsafe-head propagation.
//! Real consensus nodes expose peer-topic counts over RPC; batching is stopped before
//! selecting a future target so L1 derivation cannot substitute for unsafe propagation.

use std::time::{Duration, SystemTime};

use alloy_eips::BlockNumberOrTag;
use alloy_provider::Provider;
use base_consensus_gossip::BlockHandler;
use base_consensus_rpc::{BaseP2PApiClient, RollupNodeApiClient};
use base_system_tests::SystemTestStackBuilder;
use eyre::{Result, WrapErr};
use jsonrpsee::http_client::HttpClientBuilder;
use tokio::time::{sleep, timeout};

#[tokio::test]
async fn retired_topics_preserve_connected_peers_and_unsafe_sync() -> Result<()> {
    let system = SystemTestStackBuilder::new().with_l2_chain_id(84_538_477).build().await?;
    let builder_rpc = HttpClientBuilder::default()
        .build(system.l2_stack().builder_consensus_rpc_url().as_str())?;
    let client_rpc = HttpClientBuilder::default()
        .build(system.l2_stack().client_consensus_rpc_url().as_str())?;
    let config = builder_rpc.rollup_config().await?;
    let isthmus = config.upgrades.isthmus_time.expect("system stack enables Isthmus");

    // Works whether the stack activates Isthmus at zero or at genesis time.
    timeout(Duration::from_secs(90), async {
        loop {
            let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_secs();
            if now >= isthmus + BlockHandler::MAX_BLOCK_AGE {
                let mut ready = true;
                for rpc in [&builder_rpc, &client_rpc] {
                    let stats = rpc.opp2p_peer_stats().await?;
                    ready &= stats.connected > 0
                        && stats.blocks_topic == 0
                        && stats.blocks_topic_v2 == 0
                        && stats.blocks_topic_v3 == 0
                        && stats.blocks_topic_v4 > 0;
                }
                if ready {
                    return Ok::<_, eyre::Error>(());
                }
            }
            sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .wrap_err("nodes did not retain V4 peering after old topics retired")??;

    // Stop production first so no in-flight batch can contain the target block.
    system.l2_stack().builder_consensus().stop_sequencer().await?;
    system.l2_stack().batcher().stop();
    let builder = system.l2_builder_provider()?;
    let client = system.l2_client_provider()?;
    let target = builder.get_block_number().await? + 5;
    system.l2_stack().builder_consensus().start_sequencer().await?;

    timeout(Duration::from_secs(60), async {
        loop {
            let status = client_rpc.sync_status().await?;
            if status.unsafe_l2.block_info.number >= target {
                assert!(status.safe_l2.block_info.number < target, "target must arrive unsafely");
                let expected = builder
                    .get_block_by_number(BlockNumberOrTag::Number(target))
                    .await?
                    .expect("builder produced target");
                let received = client
                    .get_block_by_number(BlockNumberOrTag::Number(target))
                    .await?
                    .expect("client imported target");
                assert_eq!(received.header.hash, expected.header.hash);
                for rpc in [&builder_rpc, &client_rpc] {
                    let stats = rpc.opp2p_peer_stats().await?;
                    assert!(stats.connected > 0 && stats.blocks_topic_v4 > 0);
                    assert_eq!(
                        (stats.blocks_topic, stats.blocks_topic_v2, stats.blocks_topic_v3),
                        (0, 0, 0)
                    );
                }
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .wrap_err("client stopped importing unsafe blocks after topic retirement")??;
    Ok(())
}
