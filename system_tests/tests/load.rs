//! Load tests for metering and transaction acceptance on the L2 devnet.
//!
//! These tests deploy the Simulator contract and send multiple transactions
//! to verify the network can handle sustained transaction load without failures.

use std::time::Duration;

use alloy_provider::Provider;
use alloy_signer_local::PrivateKeySigner;
use eyre::{Result, WrapErr};
use system_tests::{
    DevnetBuilder, L1_CHAIN_ID, L2_CHAIN_ID,
    config::ANVIL_ACCOUNT_1,
    http_provider,
    load::{Generator, LoadConfig, deploy, fund_contract, get_simulator_bytecode},
};
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn load_test_devnet_transaction_acceptance() -> Result<()> {
    eprintln!("Starting load test...");

    let devnet = DevnetBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .build()
        .await?;

    let l2_rpc_url = devnet.l2_rpc_url()?;

    let provider = http_provider(l2_rpc_url.as_ref())?;

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
    let contract_address = deploy(&provider, &signer, deploy_bytecode, L2_CHAIN_ID).await?;
    eprintln!("Simulator contract deployed: {contract_address}");

    fund_contract(&provider, &signer, contract_address, L2_CHAIN_ID).await?;
    eprintln!("Simulator contract funded");

    let config = LoadConfig::default()
        .with_tx_rate(2.0)
        .with_parallel(3)
        .with_duration_secs(15)
        .with_create_storage(5)
        .with_create_accounts(2)
        .with_calldata_bytes(1000);

    let generator =
        Generator::new(provider.clone(), signer, config, contract_address, L2_CHAIN_ID).await?;

    let stats = generator.stats();
    let shutdown = CancellationToken::new();

    eprintln!("Starting load generator...");
    generator.run(shutdown).await?;

    eprintln!("Load test completed: submitted={}, failed={}", stats.submitted(), stats.failed());

    assert!(
        stats.submitted() >= 10,
        "Expected at least 10 transactions to be submitted, got {}",
        stats.submitted()
    );

    assert_eq!(stats.failed(), 0, "Expected no failed transactions, but {} failed", stats.failed());
    Ok(())
}
