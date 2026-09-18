//! Fast blob qualification without waiting for the scheduled L1 fork.

use std::time::Duration;

use alloy_primitives::Address;
use base_common_genesis::RollupConfig;
use base_system_tests::GlamsterdamFixture;
use eyre::Result;
use jsonrpsee::http_client::HttpClientBuilder;
use serde_json::Value;

use super::{Acceptance, Schedule, Submissions, Transfer, TransferRequest};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned real-client blob fixture and Docker"]
async fn blob_transfer_is_safely_derived() -> Result<()> {
    Acceptance::run("blob-safe-derivation", GlamsterdamFixture::builder, async |rpc, system| {
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
        let schedule = Schedule::read(rpc, &beacon, &genesis).await?;
        let mut batches = Submissions::new(rollup.genesis.l1.number, beacon, &schedule);

        let transfer = Transfer::send(
            rpc,
            &sequencer,
            &sequencer_url,
            &l1,
            &rollup,
            TransferRequest { recipient: Address::repeat_byte(0x75), value: 3_017 },
            Duration::from_secs(90),
        )
        .await?;
        // This must recover the exact signed transaction from a complete blob channel,
        // not merely observe batcher activity or the sequencer's unsafe receipt.
        let attribution = batches.wait_for_transfer(rpc, &l1, &rollup, &transfer).await?;
        transfer
            .wait_until_safe(
                rpc,
                &sequencer,
                &verifier,
                [&sequencer_cl, &verifier_cl],
                &verifier_url,
            )
            .await?;
        Submissions::require_canonical(rpc, &l1, &attribution).await
    })
    .await
}
