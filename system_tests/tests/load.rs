//! Load tests for metering and transaction acceptance on the L2 devnet.
//!
//! These tests deploy the Simulator contract and send multiple transactions
//! to verify the network can handle sustained transaction load without failures.

use std::time::Duration;

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::Ethereum;
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use eyre::{Result, WrapErr};
use op_alloy_network::TransactionBuilder;
use op_alloy_rpc_types::OpTransactionRequest;
use system_tests::{
    DevnetBuilder, L1_CHAIN_ID, L2_CHAIN_ID,
    config::ANVIL_ACCOUNT_1,
    http_provider,
    load::{Generator, LoadConfig, simulator_deploy_bytecode},
};
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

async fn deploy_simulator(
    provider: &RootProvider<Ethereum>,
    signer: &PrivateKeySigner,
    chain_id: u64,
) -> Result<Address> {
    let deploy_bytecode = simulator_deploy_bytecode(0);

    let nonce = provider.get_transaction_count(signer.address()).await?;
    let gas_price = provider.get_gas_price().await?;

    let tx_request = OpTransactionRequest::default()
        .from(signer.address())
        .with_deploy_code(deploy_bytecode)
        .transaction_type(2)
        .with_nonce(nonce)
        .with_gas_limit(5_000_000)
        .with_max_fee_per_gas(gas_price * 2)
        .with_max_priority_fee_per_gas(1_000_000)
        .with_chain_id(chain_id);

    let tx = tx_request
        .build_typed_tx()
        .map_err(|e| eyre::eyre!("Failed to build typed tx: {:?}", e))?;

    let signature = signer.sign_hash_sync(&tx.signature_hash())?;
    let signed_tx = tx.into_signed(signature);
    let raw_tx: Bytes = signed_tx.encoded_2718().into();
    let tx_hash = *signed_tx.hash();

    eprintln!("Deploying Simulator contract... tx_hash={tx_hash}");

    let _ = provider
        .send_raw_transaction(&raw_tx)
        .await
        .wrap_err("Failed to send deploy transaction")?;

    let receipt = timeout(Duration::from_secs(60), async {
        loop {
            if let Some(receipt) = provider.get_transaction_receipt(tx_hash).await? {
                return Ok::<_, eyre::Error>(receipt);
            }
            sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .wrap_err("Deploy receipt timed out")?
    .wrap_err("Failed to get deploy receipt")?;

    let contract_address =
        receipt.contract_address.ok_or_else(|| eyre::eyre!("No contract address in receipt"))?;

    eprintln!("Simulator contract deployed: {contract_address}");

    Ok(contract_address)
}

async fn fund_simulator_contract(
    provider: &RootProvider<Ethereum>,
    signer: &PrivateKeySigner,
    contract_address: Address,
    chain_id: u64,
) -> Result<()> {
    let nonce = provider.get_transaction_count(signer.address()).await?;
    let gas_price = provider.get_gas_price().await?;

    let mut tx_request = OpTransactionRequest::default()
        .from(signer.address())
        .to(contract_address)
        .value(U256::from(1_000_000_000_000_000_000u128))
        .with_nonce(nonce)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(gas_price * 2)
        .with_max_priority_fee_per_gas(1_000_000);
    tx_request.set_chain_id(chain_id);

    let tx = tx_request
        .build_typed_tx()
        .map_err(|e| eyre::eyre!("Failed to build typed tx: {:?}", e))?;

    let signature = signer.sign_hash_sync(&tx.signature_hash())?;
    let signed_tx = tx.into_signed(signature);
    let raw_tx: Bytes = signed_tx.encoded_2718().into();
    let tx_hash = *signed_tx.hash();

    eprintln!("Funding Simulator contract... tx_hash={tx_hash}");

    let _ = provider
        .send_raw_transaction(&raw_tx)
        .await
        .wrap_err("Failed to send funding transaction")?;

    timeout(Duration::from_secs(30), async {
        loop {
            if provider.get_transaction_receipt(tx_hash).await?.is_some() {
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .wrap_err("Funding receipt timed out")??;

    eprintln!("Simulator contract funded");

    Ok(())
}

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

    let contract_address = deploy_simulator(&provider, &signer, L2_CHAIN_ID).await?;

    fund_simulator_contract(&provider, &signer, contract_address, L2_CHAIN_ID).await?;

    let config = LoadConfig::default()
        .with_tx_rate(2.0)
        .with_parallel(3)
        .with_duration_secs(15)
        .with_create_storage(5)
        .with_create_accounts(2);

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
