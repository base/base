//! Integration test for `verifier_l1_confs` — verifies that the `safe` block tag on the
//! client (validator) node is delayed when `verifier_l1_confs > 0`.
//!
//! This test reproduces the scenario from the original bug report: after setting
//! `BASE_NODE_VERIFIER_L1_CONFS`, the `safe` tag was returning the same value as the
//! sequencer (no delay), while `finalized` was correctly delayed.
//!
//! Run with:
//!   cargo test -p devnet -- verifier_l1_confs --nocapture
//!
//! Requires Docker (L1 stack runs Reth + Lighthouse containers).

use std::time::Duration;

use alloy_eips::BlockNumberOrTag;
use alloy_provider::{Provider, RootProvider};
use base_common_network::Base;
use devnet::DevnetBuilder;
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(60);
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Number of L1 confirmations required before the validator derives L2 blocks.
///
/// In the devnet, L1 slot time is 2s, so 4 confirmations = 8s of safe-head lag.
const VERIFIER_L1_CONFS: u64 = 4;

/// Wait for a provider to reach at least `min_block`.
async fn wait_for_block(provider: &RootProvider<Base>, min_block: u64) -> Result<u64> {
    timeout(BLOCK_PRODUCTION_TIMEOUT, async {
        loop {
            let block = provider.get_block_number().await?;
            if block >= min_block {
                return Ok::<_, eyre::Error>(block);
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("Block production timed out")?
}

/// Returns the block number for a given tag, or `None` if the tag has no block yet.
async fn block_number_by_tag(
    provider: &RootProvider<Base>,
    tag: BlockNumberOrTag,
) -> Result<Option<u64>> {
    Ok(provider.get_block_by_number(tag).await?.map(|b| b.header.number))
}

fn display_block(label: &str, tag: &str, number: Option<u64>) {
    match number {
        Some(n) => println!("  {label:>30}: {tag:<12} number {n:>10}"),
        None => println!("  {label:>30}: {tag:<12} (no block)"),
    }
}

/// Verifies that the client (validator) node's `safe` head is behind the builder
/// (sequencer) node's `safe` head when `verifier_l1_confs` is non-zero.
///
/// Prints output in the same format as the original bug report's `cast` loop so you
/// can visually compare builder (sequencer, no delay) vs client (validator, with delay).
#[tokio::test]
async fn safe_head_is_delayed_by_verifier_l1_confs() -> Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();

    let devnet = DevnetBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_verifier_l1_confs(VERIFIER_L1_CONFS)
        .build()
        .await?;

    let builder_url = devnet.l2_rpc_url()?;
    let client_url = devnet.l2_client_rpc_url()?;
    let builder_provider = devnet.l2_builder_provider()?;
    let client_provider = devnet.l2_client_provider()?;

    println!("\n=== Devnet started (verifier_l1_confs={VERIFIER_L1_CONFS}) ===");
    println!("  Builder (sequencer) EL RPC: {builder_url}");
    println!("  Client  (validator) EL RPC: {client_url}");

    // Wait for the builder to produce blocks.
    println!("\nWaiting for builder to reach block 20...");
    wait_for_block(&builder_provider, 20).await?;

    // Wait for the client to sync via gossip.
    println!("Waiting for client to sync...");
    timeout(Duration::from_secs(60), async {
        loop {
            let client_block = client_provider.get_block_number().await?;
            if client_block >= 10 {
                return Ok::<_, eyre::Error>(client_block);
            }
            sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .wrap_err("Client block sync timed out")??;

    // Print block numbers for all tags, matching the Slack message format.
    // Sample 5 times, 2s apart.
    println!("\n=== Comparing block tags (like `cast block <tag>`) ===\n");

    let mut safe_head_delayed = false;
    for round in 1..=5 {
        println!("--- Round {round} ---");
        for tag in ["latest", "safe", "finalized"] {
            let tag_enum = match tag {
                "latest" => BlockNumberOrTag::Latest,
                "safe" => BlockNumberOrTag::Safe,
                "finalized" => BlockNumberOrTag::Finalized,
                _ => unreachable!(),
            };

            let builder_num = block_number_by_tag(&builder_provider, tag_enum).await?;
            let client_num = block_number_by_tag(&client_provider, tag_enum).await?;

            display_block("builder (sequencer, no delay)", tag, builder_num);
            display_block("client (validator, with delay)", tag, client_num);

            if tag == "safe" {
                if let (Some(b), Some(c)) = (builder_num, client_num) {
                    let annotation = if c > 0 && b > c {
                        safe_head_delayed = true;
                        " <-- client safe head is delayed (CORRECT)"
                    } else if b == c {
                        " <-- same as builder (BUG if verifier_l1_confs > 0)"
                    } else {
                        ""
                    };
                    println!("  {annotation}");
                } else {
                    println!();
                }
            }
        }
        println!();

        if round < 5 {
            sleep(Duration::from_secs(2)).await;
        }
    }

    // Print cast commands so the user can also query manually.
    println!("=== To query manually while the test runs, use: ===\n");
    println!("for label in latest safe finalized; do");
    println!("  echo \"builder (sequencer, no delay):\" ${{label}}");
    println!("  cast block ${{label}} -r {builder_url} | grep number");
    println!("  echo \"client (validator, with delay):\" ${{label}}");
    println!("  cast block ${{label}} -r {client_url} | grep number");
    println!("done\n");

    assert!(
        safe_head_delayed,
        "Client safe head should lag behind builder safe head when verifier_l1_confs={VERIFIER_L1_CONFS}"
    );

    Ok(())
}
