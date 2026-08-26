//! Smoke test for the L3 chain: a Base L2 that settles to a Base L1.
//!
//! Brings up the in-process [`SystemTestStack`](base_system_tests::SystemTestStack) in the L3
//! profile ([`with_l3_profile`](base_system_tests::SystemTestStackBuilder::with_l3_profile)) and
//! checks end-to-end liveness of the two properties that distinguish an L3 from a plain L2:
//!
//! 1. the derivation pipeline decodes **Base-format** L1 blocks (`L1TxFormat::Base`), and
//! 2. the batcher submits batches via **calldata** data-availability — a Base parent chain has no
//!    blob DA endpoint.
//!
//! Fidelity note: the system-test harness L1 is a standard Ethereum reth + lighthouse devnet, so
//! this exercises the L3 *configuration and DA model* end-to-end, not Base-L1-native parent
//! features (L1 deposits / EIP-8130 transactions on the parent), which would require a Base
//! execution layer for L1. That lower-level decoding is covered by the component tests in
//! `base-common-network` / `base-proof` (e.g. `base_format_block_decodes_and_drops_eip8130`).

use std::time::Duration;

use alloy_provider::Provider;
use base_system_tests::{BATCHER, SystemTestRpcClient, SystemTestStackBuilder};
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(30);
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// The client's safe head must advance purely from L1 derivation within this window. This is
/// longer than block production because it requires the full loop: the batcher accumulates L2
/// blocks into a channel, submits the batch to L1, and only then can the client derive it.
const CLIENT_SYNC_TIMEOUT: Duration = Duration::from_secs(180);
/// The batcher must post at least one batch within this window. Submission lags block production:
/// the batcher accumulates L2 blocks into a channel and only submits once the channel closes.
const BATCH_SUBMISSION_TIMEOUT: Duration = Duration::from_secs(120);

#[tokio::test]
async fn l3_smoke_calldata_da_derivation() -> Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();

    // Bring up the stack configured as an L3: Base-format L1 decoding + calldata batch submission.
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_l3_profile()
        .build()
        .await
        .wrap_err("failed to start L3 system stack")?;

    let l1_provider = system.l1_provider().await?;
    let builder_provider = system.l2_builder_provider()?;
    let urls = system.urls().await?;
    let rpc = SystemTestRpcClient::new(
        &urls.l1_rpc,
        &urls.l2_builder_rpc,
        &urls.l2_client_rpc,
        &urls.l2_builder_consensus_rpc,
        &urls.l2_client_consensus_rpc,
    )?;

    // 1. The sequencer produces L2 blocks under the L3 config (Base tx-format + calldata DA).
    timeout(BLOCK_PRODUCTION_TIMEOUT, async {
        loop {
            if builder_provider.get_block_number().await? >= 3 {
                return Ok::<_, eyre::Error>(());
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("L2 sequencer did not produce blocks under the L3 profile")??;

    // 2. The validator DERIVES the chain from L1 — the core L3 signal. Check the client's SAFE
    //    head, not its unsafe head: the unsafe head advances via P2P gossip from the builder
    //    consensus and would pass even if L1 derivation were completely broken. The safe head
    //    only advances when the derivation pipeline successfully decodes the Base-format calldata
    //    batches read back from L1. A Base-format provider cannot decode an EIP-4844 blob
    //    transaction, so a healthy safe head also proves the batcher posted calldata, not blobs.
    let client_safe_head = timeout(CLIENT_SYNC_TIMEOUT, async {
        loop {
            // The consensus RPC may be briefly unavailable at startup; treat any error as
            // "not derived yet" and keep polling until the safe head advances or we time out.
            let safe_head = rpc
                .l2_client_sync_status()
                .await
                .map(|status| status.safe_l2.block_info.number)
                .unwrap_or(0);
            if safe_head > 0 {
                return Ok::<_, eyre::Error>(safe_head);
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .wrap_err(
        "client safe head did not advance from L1 under the L3 profile (Base-format derivation stalled)",
    )??;
    assert!(client_safe_head > 0, "client should derive at least one safe L2 block from L1");

    // 3. The batcher actually submitted its batches to L1. Combined with the successful
    //    Base-format client sync above, a positive nonce confirms the calldata-DA path: had the
    //    batcher posted blob transactions, the client's Base-format L1 decoding would have failed
    //    and step 2 would have timed out.
    //
    //    Poll rather than sample once: batch submission lags block production because the batcher
    //    accumulates L2 blocks into a channel and only posts once the channel closes, which can
    //    happen after the client has already advanced past block 0.
    let batcher_l1_nonce = timeout(BATCH_SUBMISSION_TIMEOUT, async {
        loop {
            let nonce = l1_provider
                .get_transaction_count(BATCHER.address)
                .await
                .wrap_err("failed to read batcher L1 nonce")?;
            if nonce > 0 {
                return Ok::<_, eyre::Error>(nonce);
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("batcher did not submit any L1 batch transaction")??;
    assert!(
        batcher_l1_nonce > 0,
        "batcher should have submitted at least one L1 batch transaction"
    );

    Ok(())
}
