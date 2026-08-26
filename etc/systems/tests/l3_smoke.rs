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
use base_system_tests::{BATCHER, SystemTestStackBuilder};
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(30);
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// The client must derive the chain purely from L1 within this window.
const CLIENT_SYNC_TIMEOUT: Duration = Duration::from_secs(90);
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
    let client_provider = system.l2_client_provider()?;

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

    // 2. The validator derives the chain from L1. This is the core L3 signal: the client's L1
    //    provider is configured for Base-format decoding, and derivation only advances if the
    //    batches it reads back from L1 decode cleanly. A Base-format provider cannot decode an
    //    EIP-4844 blob transaction, so a healthy sync also proves the batcher posted calldata,
    //    not blobs.
    let client_block = timeout(CLIENT_SYNC_TIMEOUT, async {
        loop {
            let block = client_provider.get_block_number().await?;
            if block > 0 {
                return Ok::<_, eyre::Error>(block);
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .wrap_err(
        "client did not sync from L1 under the L3 profile (Base-format derivation stalled)",
    )??;
    assert!(client_block > 0, "client should derive at least one L2 block from L1");

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
