//! Load tests for metering and transaction acceptance on the L2 devnet.
//!
//! These tests deploy the Simulator contract and send multiple transactions
//! to verify the network can handle sustained transaction load without failures.

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use devnet::{
    Devnet, DevnetBuilder,
    config::ANVIL_ACCOUNT_1,
    load::{Generator, LoadConfig, Stats, deploy, fund_contract, get_simulator_bytecode},
    smoke::DEFAULT_L2_CHAIN_ID,
};
use eyre::{Result, WrapErr};
use op_alloy_network::Optimism;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

struct TestContext {
    provider: RootProvider<Optimism>,
    signer: PrivateKeySigner,
    contract_address: Address,
    #[allow(dead_code)]
    devnet: Devnet,
}

async fn setup_test() -> Result<TestContext> {
    println!("Starting load test...");

    let devnet = DevnetBuilder::new().build().await?;

    let provider = devnet.l2_client_provider()?;

    timeout(Duration::from_secs(30), async {
        loop {
            let block = provider.get_block_number().await?;
            if block >= 1 {
                return Ok::<_, eyre::Error>(block);
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .wrap_err("Block production timed out")??;

    let private_key_hex = format!("0x{}", hex::encode(ANVIL_ACCOUNT_1.private_key.as_slice()));
    let signer: PrivateKeySigner = private_key_hex.parse()?;

    let deploy_bytecode = get_simulator_bytecode(0);
    let contract_address = deploy(&provider, &signer, deploy_bytecode, DEFAULT_L2_CHAIN_ID).await?;
    println!("Simulator contract deployed: {contract_address}");

    fund_contract(&provider, &signer, contract_address, DEFAULT_L2_CHAIN_ID).await?;
    println!("Simulator contract funded");

    Ok(TestContext { provider, signer, contract_address, devnet })
}

async fn run_load_test(ctx: &TestContext, config: LoadConfig) -> Result<Arc<Stats>> {
    let generator = Generator::new(
        ctx.provider.clone(),
        ctx.signer.clone(),
        config,
        ctx.contract_address,
        DEFAULT_L2_CHAIN_ID,
    )
    .await?;

    let stats = generator.stats();
    let shutdown = CancellationToken::new();

    println!("Starting load generator...");
    generator.run(shutdown).await?;

    println!("Load test completed: submitted={}, failed={}", stats.submitted(), stats.failed());

    Ok(stats)
}

fn assert_load_test_passed(stats: &Stats) {
    assert!(
        stats.submitted() >= 10,
        "Expected at least 10 transactions to be submitted, got {}",
        stats.submitted()
    );
    // https://github.com/base/base/issues/669
    assert!(
        stats.failed() <= 3,
        "Expected no more than 3 failed transactions, but {} failed",
        stats.failed()
    );
}

#[tokio::test]
async fn load_test_state_root_time() -> Result<()> {
    let ctx = setup_test().await?;
    let config = LoadConfig::default().with_tx_rate(2.0).with_create_accounts(100);
    let stats = run_load_test(&ctx, config).await?;
    assert_load_test_passed(&stats);
    Ok(())
}

#[tokio::test]
async fn load_test_calldata() -> Result<()> {
    let ctx = setup_test().await?;
    let config = LoadConfig::default().with_tx_rate(1.0).with_calldata_bytes(1000);
    let stats = run_load_test(&ctx, config).await?;
    assert_load_test_passed(&stats);
    Ok(())
}

#[tokio::test]
async fn load_test_storage_slot_creation() -> Result<()> {
    let ctx = setup_test().await?;
    let config = LoadConfig::default().with_tx_rate(1.0).with_create_storage(100);
    let stats = run_load_test(&ctx, config).await?;
    assert_load_test_passed(&stats);
    Ok(())
}

#[tokio::test]
async fn load_test_combined_high_parallelism() -> Result<()> {
    let ctx = setup_test().await?;
    let config = LoadConfig::default()
        .with_tx_rate(1.0)
        .with_create_storage(20)
        .with_create_accounts(10)
        .with_parallel(10);
    let stats = run_load_test(&ctx, config).await?;
    assert_load_test_passed(&stats);
    Ok(())
}
