//! Base continues producing and safely deriving real transfers across L1 Glamsterdam.

use std::{panic::AssertUnwindSafe, time::Duration};

use alloy_primitives::{Address, U256};
use base_batcher_encoder::DaType;
use base_common_genesis::RollupConfig;
use base_consensus_rpc::SyncStatusApiClient;
use base_system_tests::{GlamsterdamConfig, SystemTestStack, SystemTestStackBuilder};
use eyre::{Result, WrapErr, ensure};
use futures::FutureExt;
use jsonrpsee::http_client::HttpClientBuilder;
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};

use super::{
    activation::Schedule,
    evidence::Evidence,
    rpc::Rpc,
    submissions::Submissions,
    transfer::{Transfer, TransferRequest},
};

#[tokio::test]
#[ignore = "requires the pinned real-client Glamsterdam fixture and Docker"]
async fn glamsterdam_calldata() -> Result<()> {
    run(DaType::Calldata, "glamsterdam::glamsterdam_calldata").await
}

/// Owns setup and teardown, keeping observable acceptance expectations in `scenario`.
pub async fn run(da: DaType, case: &str) -> Result<()> {
    let mut evidence = Evidence::new(case)?;
    let rpc = Rpc::new()?;
    let system = match SystemTestStackBuilder::new()
        .with_output_dir(evidence.directory.join("runtime"))
        // The real origin selector prepares successors asynchronously, requiring two L2
        // ticks per origin. Six-second L1 slots leave headroom over unchanged 2s L2 blocks;
        // eight minimal epochs retain the original 384-second pre-fork window.
        .with_l1_glamsterdam(GlamsterdamConfig { activation_epoch: 8, slot_duration: 6 })
        .with_batcher_da_type(da)
        .build()
        .await
    {
        Ok(system) => system,
        Err(error) => {
            evidence.document["error"] = json!(format!("setup: {error:#}"));
            evidence.checkpoint()?;
            return Err(error);
        }
    };
    let result = AssertUnwindSafe(scenario(&rpc, &system, &mut evidence))
        .catch_unwind()
        .await
        .unwrap_or_else(|panic| {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic");
            Err(eyre::eyre!("scenario panicked: {message}"))
        });
    evidence.document["phase"] = json!("diagnostics_before_cleanup");
    evidence.document["error"] = json!(result.as_ref().err().map_or_else(
        || "checks completed; cleanup pending".to_string(),
        |error| format!("{error:#}"),
    ));
    // Do not let one diagnostic failure skip the remaining captures or scoped teardown.
    let checkpoint = evidence.checkpoint();
    let rpc_diagnostics = evidence.rpc_diagnostics(&rpc, &system).await;
    let component_logs = system.l1_stack().capture_diagnostics(&evidence.directory).await;
    let cleanup = timeout(Duration::from_secs(60), system.shutdown())
        .await
        .wrap_err("scoped system shutdown timed out")
        .and_then(|result| result);
    evidence.document["diagnostics"] = json!({
        "checkpoint_error": checkpoint.as_ref().err().map(|error| format!("{error:#}")),
        "rpc_error": rpc_diagnostics.as_ref().err().map(|error| format!("{error:#}")),
        "component_error": component_logs.as_ref().err().map(|error| format!("{error:#}")),
    });
    evidence.document["cleanup"] = json!({
        "completed": cleanup.is_ok(),
        "error": cleanup.as_ref().err().map(|error| format!("{error:#}")),
    });
    let outcome = result.and(checkpoint).and(rpc_diagnostics).and(component_logs).and(cleanup);
    evidence.document["phase"] = json!("complete");
    evidence.document["status"] = json!(if outcome.is_ok() { "passed" } else { "failed" });
    evidence.document["error"] =
        outcome.as_ref().err().map_or(Value::Null, |error| json!(format!("{error:#}")));
    evidence.checkpoint()?;
    outcome
}

/// Setup → transfer → batch → safe agreement → activation → repeat, with four explicit checks.
pub async fn scenario(rpc: &Rpc, system: &SystemTestStack, evidence: &mut Evidence) -> Result<()> {
    evidence.configs(system)?;
    ensure!(
        std::env::var("BASE_GLAMSTERDAM_INJECT_FAILURE").as_deref() != Ok("after_setup"),
        "deliberate failure after setup, before acceptance assertions"
    );
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
    let rollup: RollupConfig = serde_json::from_str(&system.l2_deployment().read_rollup_config()?)?;
    let genesis: Value = serde_json::from_str(&system.l1_genesis().read_el_genesis()?)?;
    let l2_genesis: Value = serde_json::from_str(&system.l2_deployment().read_genesis()?)?;

    // 1. A real matched future EL timestamp / CL epoch schedule; never post-fork-only startup.
    let schedule = Schedule::read(rpc, &beacon, &genesis).await?;
    let initial = rpc.header(&l1, json!("latest")).await?;
    ensure!(initial.header.timestamp < schedule.activation, "setup missed pre-fork L1 window");
    initial.without_amsterdam()?;
    evidence.document["activation"] = json!({"schedule": schedule, "initial": initial});
    evidence.checkpoint()?;

    // 4. L1 scheduling must not activate Amsterdam or the future-rule testing gate on L2.
    ensure!(l2_genesis["config"]["amsterdamTime"].is_null(), "L2 Amsterdam must not be configured");
    ensure!(
        rollup.upgrades.base.zenith.is_none(),
        "L2 Zenith future-rule gate must remain unscheduled"
    );

    // 2 + 3. A successful pre-fork value transfer, its decoded batch, and matching safe blocks.
    // Start at genesis so an already-open channel cannot hide earlier contributing frames.
    let mut batches = Submissions::new(rollup.genesis.l1.number);
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
    evidence.document["transfers"]["pre"] = json!(pre);
    evidence.checkpoint()?;
    let pre_batch = batches.wait_for_transfer(rpc, &l1, &rollup, &pre).await?;
    ensure!(
        !pre_batch.submissions.is_empty()
            && pre_batch
                .submissions
                .iter()
                .all(|submission| submission.block.timestamp < schedule.activation),
        "pre-fork transfer's batch channel was not completely submitted before activation"
    );
    evidence.document["batches"]["pre"] = json!(pre_batch);
    evidence.document["safe_blocks"]["pre"] = pre
        .wait_until_safe(rpc, &sequencer, &verifier, [&sequencer_cl, &verifier_cl], &verifier_url)
        .await?;
    ensure!(
        rpc.header(&l1, json!("latest")).await?.header.timestamp < schedule.activation,
        "pre-fork transaction was not safe before activation"
    );
    evidence.checkpoint()?;

    // 1. Authenticate the actual last pre-fork and first post-fork headers and their linkage.
    let (before, after) = schedule.wait_for_boundary(rpc, &l1, initial).await?;
    evidence.document["activation"]["before"] = json!(before);
    evidence.document["activation"]["after"] = json!(after);
    evidence.checkpoint()?;
    let l1_slotnum = rpc.slotnum(&l1, after.hash).await?;
    ensure!(l1_slotnum.get("error").is_none(), "post-Amsterdam L1 SLOTNUM failed: {l1_slotnum}");
    let slotnum: U256 = serde_json::from_value(l1_slotnum["result"].clone())?;
    ensure!(
        Some(slotnum.to::<u64>()) == after.header.slot_number,
        "L1 SLOTNUM disagrees with authenticated header"
    );

    // Wait for a genuine post-fork origin, not just post-fork wall-clock submission time.
    let mut last_origin = Value::Null;
    timeout(Duration::from_secs(60), async {
        loop {
            let status = sequencer_cl.sync_status().await?;
            last_origin = json!(status);
            if status.unsafe_l2.l1_origin.number >= after.header.number {
                return Ok::<_, eyre::Report>(());
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .wrap_err_with(|| {
        format!(
            "sequencer did not adopt L1 origin {} ({}); last status: {last_origin}",
            after.header.number, after.hash
        )
    })??;
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
    evidence.document["transfers"]["post"] = json!(post);
    evidence.checkpoint()?;
    let post_batch = batches.wait_for_transfer(rpc, &l1, &rollup, &post).await?;
    ensure!(
        !post_batch.submissions.is_empty()
            && post_batch
                .submissions
                .iter()
                .all(|submission| submission.block.timestamp >= schedule.activation),
        "post-fork transfer's batch channel contains pre-fork submissions"
    );
    evidence.document["batches"]["post"] = json!(post_batch);
    evidence.document["safe_blocks"]["post"] = post
        .wait_until_safe(rpc, &sequencer, &verifier, [&sequencer_cl, &verifier_cl], &verifier_url)
        .await?;
    evidence.checkpoint()?;

    // 4. The same opcode remains inactive on both unchanged L2 nodes at both receipt blocks.
    let mut opcode_checks = Vec::new();
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
            opcode_checks
                .push(json!({"url": url, "block_hash": header.hash, "response": response}));
        }
    }
    evidence.document["l2_rules"] = json!({"amsterdam_configured": false, "genesis_config": l2_genesis["config"], "l1_slotnum": l1_slotnum, "l2_slotnum": opcode_checks, "headers_omit_amsterdam": true});

    // 1. Gloas actually finalized consensus blocks and advanced real EL finality past the fork.
    evidence.document["activation"]["finality"] =
        schedule.wait_for_finality(rpc, &l1, &beacon).await?;
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
    evidence.checkpoint()?;
    Ok(())
}
