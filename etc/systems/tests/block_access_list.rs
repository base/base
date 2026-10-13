//! System test for EIP-7928 block access list commitments.
//!
//! A Denim-from-genesis stack exercises the real sequencer and follower paths: the sequencer must
//! commit every block header to the block access list its execution produced, and a follower must
//! import the same block hash, which it can only do by regenerating the list and matching it
//! against the header commitment.

use std::time::Duration;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use alloy_provider::Provider;
use base_system_tests::{SystemTestProviderExt, SystemTestStackBuilder};
use eyre::Result;

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84_538_453;
const BLOCK_TIMEOUT: Duration = Duration::from_secs(60);

#[tokio::test(flavor = "multi_thread")]
async fn denim_blocks_commit_to_a_block_access_list_and_followers_accept_them() -> Result<()> {
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_base_cobalt_activation_block(0)
        .with_base_denim_activation_block(0)
        .build()
        .await?;

    let builder = system.l2_builder_provider()?;
    let follower = system.l2_client_provider()?;

    let height = builder.wait_for_block(3, BLOCK_TIMEOUT).await?;
    let block = builder
        .get_block_by_number(BlockNumberOrTag::Number(height))
        .await?
        .expect("the builder must serve the block it produced");

    let commitment = block
        .header
        .inner
        .inner
        .block_access_list_hash
        .expect("a Denim block must commit to a block access list");
    assert_ne!(commitment, B256::ZERO, "the commitment must not be the zero hash");

    // A follower rejects a block whose commitment does not match the list it recomputed, so
    // converging on the same hash proves the follower regenerated and validated the list.
    follower.wait_for_convergence(&builder, height, BLOCK_TIMEOUT).await?;

    Ok(())
}
