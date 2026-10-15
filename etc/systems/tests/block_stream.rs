//! End-to-end test for the post-Denim native block stream.

use std::{collections::BTreeMap, time::Duration};

use alloy_eips::BlockNumberOrTag;
use alloy_provider::Provider;
use base_common_rpc_types::BlockStreamPayloadV1;
use base_system_tests::SystemTestStackBuilder;
use eyre::{OptionExt, Result, WrapErr};
use futures::StreamExt;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const DENIM_ACTIVATION_BLOCK: u64 = 1;
const VERIFIED_BLOCKS: u64 = 9;
const STREAM_TIMEOUT: Duration = Duration::from_secs(45);

#[tokio::test]
async fn basic_builder_streams_every_block() -> Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();

    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_base_denim_activation_block(DENIM_ACTIVATION_BLOCK)
        .with_basic_payload_builder()
        .build()
        .await?;
    let builder = system.l2_builder_provider()?;

    // Verify a live window starting at the first block received, so the test does not depend on
    // the publisher's replay buffer still holding early blocks.
    let (mut messages, _) = connect_async(system.l2_stack().builder().flashblocks_url()).await?;

    let mut blocks = BTreeMap::new();
    timeout(STREAM_TIMEOUT, async {
        while blocks.len() < VERIFIED_BLOCKS as usize {
            let Message::Text(text) = messages.next().await.ok_or_eyre("stream closed")?? else {
                continue;
            };
            let payload: BlockStreamPayloadV1 = serde_json::from_str(&text)?;
            blocks.insert(payload.block_number, payload);
        }
        Ok::<_, eyre::Error>(())
    })
    .await
    .wrap_err("timed out waiting for streamed blocks")??;

    let first_block = *blocks.keys().next().ok_or_eyre("no blocks streamed")?;
    let mut previous_timestamp_ms = None;
    for number in first_block..first_block + VERIFIED_BLOCKS {
        let streamed = blocks.get(&number).ok_or_eyre(format!("block {number} not streamed"))?;
        let canonical = builder
            .get_block_by_number(BlockNumberOrTag::Number(number))
            .await?
            .ok_or_eyre("builder block missing")?;
        assert_eq!(streamed.header.hash, canonical.header.hash, "block {number} hash mismatch");
        assert_eq!(streamed.transactions.len(), canonical.transactions.len());
        assert_eq!(streamed.receipts.len(), streamed.transactions.len());

        let timestamp_ms = streamed
            .header
            .timestamp_ms
            .ok_or_eyre(format!("block {number} has no timestampMs"))?;
        if let Some(previous) = previous_timestamp_ms {
            assert_eq!(timestamp_ms - previous, 200, "block {number} cadence mismatch");
        }
        previous_timestamp_ms = Some(timestamp_ms);
    }

    Ok(())
}
