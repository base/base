//! Contract deployment and funding utilities.
//!
//! Provides generalized functions for deploying contracts and funding them,
//! reusable across different load testing scenarios.

use std::time::Duration;

use alloy_consensus::SignableTransaction;
use alloy_eips::{BlockNumberOrTag, eip2718::Encodable2718};
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use eyre::{Result, WrapErr};
use op_alloy_network::{Optimism, TransactionBuilder};
use op_alloy_rpc_types::OpTransactionRequest;
use tokio::time::{sleep, timeout};

/// Default gas limit for contract deployments.
const DEFAULT_DEPLOY_GAS_LIMIT: u64 = 5_000_000;

/// Default gas limit for simple ETH transfers.
const DEFAULT_TRANSFER_GAS_LIMIT: u64 = 21_000;

/// Default funding amount: 1 ETH.
const DEFAULT_FUNDING_AMOUNT: u128 = 1_000_000_000_000_000_000;

/// Default receipt timeout in seconds.
const DEFAULT_RECEIPT_TIMEOUT_SECS: u64 = 60;

/// Deploys a contract and returns the deployed contract address.
///
/// # Arguments
/// * `provider` - The RPC provider to use.
/// * `signer` - The signer to deploy with (must have sufficient balance).
/// * `bytecode` - The deployment bytecode (init code + constructor args if any).
/// * `chain_id` - The chain ID to deploy on.
///
/// # Returns
/// The deployed contract address.
pub async fn deploy(
    provider: &RootProvider<Optimism>,
    signer: &PrivateKeySigner,
    bytecode: Bytes,
    chain_id: u64,
) -> Result<Address> {
    deploy_with_options(
        provider,
        signer,
        bytecode,
        chain_id,
        DEFAULT_DEPLOY_GAS_LIMIT,
        DEFAULT_RECEIPT_TIMEOUT_SECS,
    )
    .await
}

/// Deploys a contract with custom gas limit and timeout options.
///
/// # Arguments
/// * `provider` - The RPC provider to use.
/// * `signer` - The signer to deploy with (must have sufficient balance).
/// * `bytecode` - The deployment bytecode (init code + constructor args if any).
/// * `chain_id` - The chain ID to deploy on.
/// * `gas_limit` - The gas limit for the deployment transaction.
/// * `receipt_timeout_secs` - Timeout for waiting for the receipt.
///
/// # Returns
/// The deployed contract address.
pub async fn deploy_with_options(
    provider: &RootProvider<Optimism>,
    signer: &PrivateKeySigner,
    bytecode: Bytes,
    chain_id: u64,
    gas_limit: u64,
    receipt_timeout_secs: u64,
) -> Result<Address> {
    let nonce = provider
        .get_transaction_count(signer.address())
        .block_id(BlockNumberOrTag::Latest.into())
        .await?;
    let gas_price = provider.get_gas_price().await?;

    let tx_request = OpTransactionRequest::default()
        .from(signer.address())
        .with_deploy_code(bytecode)
        .transaction_type(2)
        .with_nonce(nonce)
        .with_gas_limit(gas_limit)
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

    tracing::debug!(%tx_hash, "Deploying contract...");

    let _ = provider
        .send_raw_transaction(&raw_tx)
        .await
        .wrap_err("Failed to send deploy transaction")?;

    let receipt = timeout(Duration::from_secs(receipt_timeout_secs), async {
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

    let contract_address = receipt
        .inner
        .contract_address
        .ok_or_else(|| eyre::eyre!("No contract address in receipt"))?;

    // Wait for nonce to increment before returning to avoid race conditions
    // where subsequent transactions query a stale nonce.
    let expected_nonce = nonce + 1;
    timeout(Duration::from_secs(10), async {
        loop {
            let current = provider
                .get_transaction_count(signer.address())
                .block_id(BlockNumberOrTag::Latest.into())
                .await?;
            if current >= expected_nonce {
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .wrap_err("Nonce update timed out")?
    .wrap_err("Failed waiting for nonce update")?;

    tracing::debug!(%contract_address, "Contract deployed");

    Ok(contract_address)
}

/// Funds a contract with ETH.
///
/// # Arguments
/// * `provider` - The RPC provider to use.
/// * `signer` - The signer to send from (must have sufficient balance).
/// * `to` - The address to fund.
/// * `chain_id` - The chain ID.
///
/// # Returns
/// `Ok(())` if the funding was successful.
pub async fn fund_contract(
    provider: &RootProvider<Optimism>,
    signer: &PrivateKeySigner,
    to: Address,
    chain_id: u64,
) -> Result<()> {
    fund_contract_with_amount(provider, signer, to, chain_id, U256::from(DEFAULT_FUNDING_AMOUNT))
        .await
}

/// Funds a contract with a specific amount of ETH.
///
/// # Arguments
/// * `provider` - The RPC provider to use.
/// * `signer` - The signer to send from (must have sufficient balance).
/// * `to` - The address to fund.
/// * `chain_id` - The chain ID.
/// * `amount` - The amount of wei to send.
///
/// # Returns
/// `Ok(())` if the funding was successful.
pub async fn fund_contract_with_amount(
    provider: &RootProvider<Optimism>,
    signer: &PrivateKeySigner,
    to: Address,
    chain_id: u64,
    amount: U256,
) -> Result<()> {
    let nonce = provider
        .get_transaction_count(signer.address())
        .block_id(BlockNumberOrTag::Latest.into())
        .await?;
    let gas_price = provider.get_gas_price().await?;

    let mut tx_request = OpTransactionRequest::default()
        .from(signer.address())
        .to(to)
        .value(amount)
        .with_nonce(nonce)
        .with_gas_limit(DEFAULT_TRANSFER_GAS_LIMIT)
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

    tracing::debug!(%tx_hash, %to, %amount, "Funding address...");

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

    // Wait for nonce to increment before returning.
    let expected_nonce = nonce + 1;
    timeout(Duration::from_secs(10), async {
        loop {
            let current = provider
                .get_transaction_count(signer.address())
                .block_id(BlockNumberOrTag::Latest.into())
                .await?;
            if current >= expected_nonce {
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .wrap_err("Nonce update timed out")?
    .wrap_err("Failed waiting for nonce update")?;

    tracing::debug!(%to, "Address funded");

    Ok(())
}
