//! Base continues producing and safely deriving real transfers across L1 Glamsterdam.

use std::{
    panic::AssertUnwindSafe,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{Address, U256};
use base_batcher_service::BatcherConfig;
use base_common_genesis::RollupConfig;
use base_system_tests::{GlamsterdamFixture, SystemTestStack};
use eyre::{Result, WrapErr, ensure};
use futures::FutureExt;
use jsonrpsee::http_client::HttpClientBuilder;
use serde_json::{Value, json};
use tokio::time::timeout;
use tracing::info;

use super::{
    activation::Schedule,
    rpc::Rpc,
    submissions::Submissions,
    transfer::{Transfer, TransferRequest},
};

#[tokio::test]
#[ignore = "requires the pinned real-client Glamsterdam fixture and Docker"]
async fn blob_transfers_remain_safe_across_glamsterdam() -> Result<()> {
    let artifacts = GlamsterdamScenario::artifact_directory()?;
    // Construct all fallible test-side infrastructure before starting processes that need cleanup.
    let rpc = Rpc::new()?;
    // Short channels make pre/post attribution bounded; the production blob DA default is unchanged.
    let mut batcher = BatcherConfig::default();
    batcher.encoder_config.max_channel_duration = 2;
    batcher.encoder_config.sub_safety_margin = 0;
    batcher.tx_manager.receipt_query_interval = Duration::from_secs(1);
    let system =
        GlamsterdamFixture::builder(&artifacts).await?.with_batcher_config(batcher).build().await?;
    let result =
        AssertUnwindSafe(GlamsterdamScenario::new(&rpc, &system).run()).catch_unwind().await;
    let diagnostics = system.l1_stack().capture_diagnostics(&artifacts).await;
    let shutdown = timeout(Duration::from_secs(60), system.shutdown())
        .await
        .wrap_err("system shutdown timed out")
        .and_then(|result| result);
    let scenario = match result {
        Ok(result) => result,
        Err(panic) => {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic");
            Err(eyre::eyre!("scenario panicked: {message}"))
        }
    };
    info!(
        artifacts = %artifacts.display(),
        scenario_ok = scenario.is_ok(),
        diagnostics_ok = diagnostics.is_ok(),
        shutdown_ok = shutdown.is_ok(),
        "Glamsterdam blob acceptance summary"
    );
    let mut failures = Vec::new();
    if let Err(error) = scenario {
        failures.push(format!("scenario: {error:#}"));
    }
    if let Err(error) = diagnostics {
        failures.push(format!("diagnostics: {error:#}"));
    }
    if let Err(error) = shutdown {
        failures.push(format!("shutdown: {error:#}"));
    }
    ensure!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}

/// Acceptance workflow over one running real-client fixture.
#[derive(Debug)]
pub struct GlamsterdamScenario<'a> {
    /// Deadline-bounded raw RPC client.
    pub rpc: &'a Rpc,
    /// Running fixture whose endpoints and generated configs are asserted.
    pub system: &'a SystemTestStack,
}

impl<'a> GlamsterdamScenario<'a> {
    /// Binds the explicit test client and already-running system.
    pub const fn new(rpc: &'a Rpc, system: &'a SystemTestStack) -> Self {
        Self { rpc, system }
    }

    /// Creates a unique fixture directory for generated config and bounded diagnostics.
    pub fn artifact_directory() -> Result<PathBuf> {
        let parent = std::env::var_os("BASE_ACCEPTANCE_ARTIFACTS")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&parent)?;
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = parent.join(format!("glamsterdam-blob-{}-{nanos}", std::process::id()));
        std::fs::create_dir(&path)?;
        Ok(path)
    }

    /// Runs setup, both explicitly configured transfer phases, boundary checks, and finality.
    pub async fn run(&self) -> Result<()> {
        let rpc = self.rpc;
        let system = self.system;
        let l1 = system.l1_rpc_url().await?.to_string();
        let beacon = system.l1_stack().beacon_url().await?;
        let sequencer_url = system.l2_rpc_url()?.to_string();
        let verifier_url = system.l2_client_rpc_url()?.to_string();
        let sequencer = system.l2_builder_provider()?;
        let verifier = system.l2_client_provider()?;
        let sequencer_cl = HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(10))
            .build(system.l2_stack().builder_consensus_rpc_url())?;
        let verifier_cl = HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(10))
            .build(system.l2_stack().client_consensus_rpc_url())?;
        let rollup: RollupConfig =
            serde_json::from_str(&system.l2_deployment().read_rollup_config()?)?;
        let genesis: Value = serde_json::from_str(&system.l1_genesis().read_el_genesis()?)?;
        let l2_genesis: Value = serde_json::from_str(&system.l2_deployment().read_genesis()?)?;

        // 1. A real matched future EL timestamp / CL epoch schedule; never post-fork-only startup.
        let schedule = Schedule::read(rpc, &beacon, &genesis).await?;
        let initial = rpc.header(&l1, json!("latest")).await?;
        ensure!(initial.header.timestamp < schedule.activation, "setup missed pre-fork L1 window");
        initial.without_amsterdam()?;

        // 4. L1 scheduling must not activate Amsterdam or the future-rule testing gate on L2.
        ensure!(
            l2_genesis["config"]["amsterdamTime"].is_null(),
            "L2 Amsterdam must not be configured"
        );
        ensure!(
            rollup.upgrades.base.zenith.is_none(),
            "L2 Zenith future-rule gate must remain unscheduled"
        );

        // 2 + 3. A successful pre-fork value transfer, its decoded batch, and matching safe blocks.
        // Start at genesis so an already-open channel cannot hide earlier contributing frames.
        let mut batches = Submissions::new(rollup.genesis.l1.number, beacon.clone(), &schedule);
        let pre = Transfer::send(
            rpc,
            &sequencer,
            &sequencer_url,
            &l1,
            &rollup,
            TransferRequest { recipient: Address::repeat_byte(0x73), value: 1_337 },
            Duration::from_secs(90),
        )
        .await?;
        ensure!(
            pre.l1_origin.header.timestamp < schedule.activation,
            "pre-fork transfer used a post-fork origin"
        );
        pre.block.without_amsterdam()?;
        let pre_batch = batches.wait_for_transfer(rpc, &l1, &rollup, &pre).await?;
        ensure!(
            !pre_batch.submissions.is_empty()
                && pre_batch
                    .submissions
                    .iter()
                    .all(|submission| submission.block.timestamp < schedule.activation),
            "pre-fork transfer's batch channel was not completely submitted before activation"
        );
        pre.wait_until_safe(
            rpc,
            &sequencer,
            &verifier,
            [&sequencer_cl, &verifier_cl],
            &verifier_url,
        )
        .await?;
        ensure!(
            rpc.header(&l1, json!("latest")).await?.header.timestamp < schedule.activation,
            "pre-fork transaction was not safe before activation"
        );

        // 1. Authenticate the actual last pre-fork and first post-fork headers and their linkage.
        let (before, after) = schedule.wait_for_boundary(rpc, &l1, initial).await?;
        let l1_slotnum = rpc.slotnum(&l1, after.hash).await?;
        ensure!(
            l1_slotnum.get("error").is_none(),
            "post-Amsterdam L1 SLOTNUM failed: {l1_slotnum}"
        );
        let slotnum: U256 = serde_json::from_value(l1_slotnum["result"].clone())?;
        ensure!(
            Some(slotnum.to::<u64>()) == after.header.slot_number,
            "L1 SLOTNUM disagrees with authenticated header"
        );

        // Wait for a genuine post-fork origin, not just post-fork wall-clock submission time.
        rpc.wait_for_l1_origin(
            &sequencer_cl,
            &l1,
            after.header.number,
            after.hash,
            Duration::from_secs(60),
        )
        .await?;
        let post = Transfer::send(
            rpc,
            &sequencer,
            &sequencer_url,
            &l1,
            &rollup,
            TransferRequest { recipient: Address::repeat_byte(0x74), value: 2_003 },
            Duration::from_secs(90),
        )
        .await?;
        ensure!(
            post.l1_origin.header.timestamp >= schedule.activation,
            "post-fork transfer still references pre-fork L1"
        );
        post.block.without_amsterdam()?;
        let post_batch = batches.wait_for_transfer(rpc, &l1, &rollup, &post).await?;
        ensure!(
            !post_batch.submissions.is_empty()
                && post_batch
                    .submissions
                    .iter()
                    .all(|submission| submission.block.timestamp >= schedule.activation),
            "post-fork transfer's batch channel contains pre-fork submissions"
        );
        post.wait_until_safe(
            rpc,
            &sequencer,
            &verifier,
            [&sequencer_cl, &verifier_cl],
            &verifier_url,
        )
        .await?;

        // 4. The same opcode remains inactive on both unchanged L2 nodes at both receipt blocks.
        for url in [&sequencer_url, &verifier_url] {
            for transfer in [&pre, &post] {
                let response = rpc.slotnum(url, transfer.block.hash).await?;
                Rpc::require_inactive_slotnum(&response)?;
                let header =
                    rpc.header(url, json!(format!("{:#x}", transfer.block.header.number))).await?;
                ensure!(
                    header.hash == transfer.block.hash,
                    "L2 transfer block changed during rules check"
                );
                header.without_amsterdam()?;
            }
        }
        // 1. Gloas actually finalized consensus blocks and advanced real EL finality past the fork.
        schedule.wait_for_finality(rpc, &l1, &beacon).await?;
        let required_finalized = pre_batch
            .submissions
            .iter()
            .chain(&post_batch.submissions)
            .map(|submission| submission.block.number)
            .chain([after.header.number])
            .max()
            .expect("boundary always supplies a block");
        let finalized = schedule
            .wait_for_finalized_height(rpc, &l1, required_finalized, Duration::from_secs(180))
            .await?;
        for boundary in [&before, &after] {
            ensure!(
                rpc.header(&l1, json!(format!("{:#x}", boundary.header.number))).await?.hash
                    == boundary.hash,
                "finalized chain replaced an observed fork boundary header"
            );
        }
        for attribution in [&pre_batch, &post_batch] {
            Submissions::require_canonical(rpc, &l1, attribution).await?;
        }
        info!(
            pre_tx = %pre.transaction_hash,
            post_tx = %post.transaction_hash,
            boundary = after.header.number,
            finalized_head = finalized.header.number,
            pre_origin = %pre.l1_origin.hash,
            post_origin = %post.l1_origin.hash,
            pre_safe = %pre.block.hash,
            post_safe = %post.block.hash,
            pre_submission = %pre_batch.submissions.last().expect("attributed channel has submissions").transaction["hash"],
            post_submission = %post_batch.submissions.last().expect("attributed channel has submissions").transaction["hash"],
            "blob transfers safely derived across Glamsterdam"
        );
        Ok(())
    }
}
