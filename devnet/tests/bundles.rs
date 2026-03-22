//! End-to-end tests for `eth_sendBundle`.

use std::time::Duration;

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_alloy_network::Base;
use base_alloy_rpc_types::OpTransactionRequest;
use base_tx_forwarding::TxForwardingConfig;
use base_txpool::{MAX_BUNDLE_ADVANCE_BLOCKS, unix_time_millis};
use devnet::{DevnetBuilder, config::ANVIL_ACCOUNT_1};
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(30);
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);
const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BundleRequest {
    txs: Vec<Bytes>,
    block_number: Option<u64>,
    min_timestamp: Option<u64>,
    max_timestamp: Option<u64>,
    reverting_tx_hashes: Option<Vec<B256>>,
    replacement_uuid: Option<String>,
    builders: Option<Vec<String>>,
}

fn create_signed_eip1559_tx(
    signer: &PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    recipient: Address,
) -> Result<(Address, Bytes, B256)> {
    let sender = signer.address();

    let tx_request = OpTransactionRequest::default()
        .from(sender)
        .to(recipient)
        .value(U256::from(1_000_000_000u64))
        .transaction_type(2)
        .with_gas_limit(21000)
        .with_max_fee_per_gas(1_000_000_000)
        .with_max_priority_fee_per_gas(1_000_000)
        .with_chain_id(chain_id)
        .with_nonce(nonce);

    let tx = tx_request
        .build_typed_tx()
        .map_err(|e| eyre::eyre!("invalid transaction request: {e:?}"))?;
    let signature = signer.sign_hash_sync(&tx.signature_hash())?;
    let signed_tx = tx.into_signed(signature);
    let tx_hash = *signed_tx.hash();
    let raw_tx: Bytes = signed_tx.encoded_2718().into();

    Ok((sender, raw_tx, tx_hash))
}

async fn wait_for_block(provider: &RootProvider<Base>, min_block: u64) -> Result<u64> {
    let result = timeout(BLOCK_PRODUCTION_TIMEOUT, async {
        loop {
            let block = provider.get_block_number().await?;
            if block >= min_block {
                return Ok::<_, eyre::Error>(block);
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("Block production timed out")??;

    Ok(result)
}

async fn wait_for_balance(provider: &RootProvider<Base>, address: Address) -> Result<()> {
    timeout(Duration::from_secs(15), async {
        loop {
            let balance = provider.get_balance(address).await?;
            if balance > U256::ZERO {
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .wrap_err("Timed out waiting for balance")??;

    Ok(())
}

#[tokio::test]
async fn test_send_bundle_accepts_valid_bundle() -> Result<()> {
    let devnet = DevnetBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_tx_forwarding(TxForwardingConfig::new(vec![]))
        .build()
        .await?;

    let builder_provider = devnet.l2_builder_provider()?;
    let client_provider = devnet.l2_client_provider()?;

    wait_for_block(&builder_provider, 2).await?;
    wait_for_block(&client_provider, 2).await?;

    let private_key_hex = format!("0x{}", hex::encode(ANVIL_ACCOUNT_1.private_key.as_slice()));
    let signer: PrivateKeySigner = private_key_hex.parse()?;
    let sender = signer.address();

    wait_for_balance(&client_provider, sender).await?;

    let nonce = client_provider.get_transaction_count(sender).await?;
    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;
    let (_, raw_tx, expected_tx_hash) =
        create_signed_eip1559_tx(&signer, L2_CHAIN_ID, nonce, recipient)?;

    let current_block = builder_provider.get_block_number().await?;
    // Use +3 to give enough lead time for the bundle to propagate from client
    // to builder before the target block is built.
    let target_block = current_block + 3;

    let request = BundleRequest {
        txs: vec![raw_tx],
        block_number: Some(target_block),
        min_timestamp: None,
        max_timestamp: None,
        reverting_tx_hashes: None,
        replacement_uuid: None,
        builders: None,
    };

    let rpc_client = RpcClient::builder().http(devnet.l2_client_rpc_url()?);
    let bundle_hash: B256 = rpc_client.request("eth_sendBundle", (request,)).await?;
    assert_eq!(bundle_hash, expected_tx_hash, "Bundle returned unexpected hash");

    let receipt = timeout(TX_RECEIPT_TIMEOUT, async {
        loop {
            if let Some(receipt) =
                builder_provider.get_transaction_receipt(expected_tx_hash).await?
            {
                return Ok::<_, eyre::Error>(receipt);
            }
            sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .wrap_err("Transaction receipt timed out")?
    .wrap_err("Failed to get transaction receipt")?;

    assert_eq!(receipt.inner.transaction_hash, expected_tx_hash);
    assert_eq!(receipt.inner.block_number, Some(target_block));
    assert_eq!(receipt.inner.from, sender);
    assert_eq!(receipt.inner.to, Some(recipient));

    Ok(())
}

#[tokio::test]
async fn test_send_bundle_rejects_invalid_bundle() -> Result<()> {
    let devnet = DevnetBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_tx_forwarding(TxForwardingConfig::new(vec![]))
        .build()
        .await?;

    let client_provider = devnet.l2_client_provider()?;
    wait_for_block(&client_provider, 2).await?;

    let rpc_client = RpcClient::builder().http(devnet.l2_client_rpc_url()?);

    let empty_request = BundleRequest {
        txs: vec![],
        block_number: None,
        min_timestamp: None,
        max_timestamp: None,
        reverting_tx_hashes: None,
        replacement_uuid: None,
        builders: None,
    };

    let empty_result: Result<B256, _> =
        rpc_client.request("eth_sendBundle", (empty_request,)).await;
    let empty_err = empty_result.expect_err("Expected empty bundle to fail");
    let empty_err_str = empty_err.to_string();
    assert!(
        empty_err_str.contains("exactly 1 transaction") || empty_err_str.contains("-32602"),
        "Unexpected error for empty bundle: {empty_err_str}"
    );

    let current_block = client_provider.get_block_number().await?;
    let far_block = current_block + MAX_BUNDLE_ADVANCE_BLOCKS + 1;
    let far_request = BundleRequest {
        txs: vec![Bytes::from_static(b"tx")],
        block_number: Some(far_block),
        min_timestamp: None,
        max_timestamp: None,
        reverting_tx_hashes: None,
        replacement_uuid: None,
        builders: None,
    };

    let far_result: Result<B256, _> = rpc_client.request("eth_sendBundle", (far_request,)).await;
    let far_err = far_result.expect_err("Expected far-future block bundle to fail");
    let far_err_str = far_err.to_string();
    assert!(
        far_err_str.contains("too far ahead") || far_err_str.contains("-32602"),
        "Unexpected error for far-future bundle: {far_err_str}"
    );

    Ok(())
}

#[tokio::test]
async fn test_send_bundle_included_only_in_target_block() -> Result<()> {
    let devnet = DevnetBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_tx_forwarding(TxForwardingConfig::new(vec![]))
        .build()
        .await?;

    let builder_provider = devnet.l2_builder_provider()?;
    let client_provider = devnet.l2_client_provider()?;

    wait_for_block(&builder_provider, 2).await?;
    wait_for_block(&client_provider, 2).await?;

    let private_key_hex = format!("0x{}", hex::encode(ANVIL_ACCOUNT_1.private_key.as_slice()));
    let signer: PrivateKeySigner = private_key_hex.parse()?;
    let sender = signer.address();

    wait_for_balance(&client_provider, sender).await?;

    let nonce = client_provider.get_transaction_count(sender).await?;
    let recipient: Address = "0x000000000000000000000000000000000000bEEF".parse()?;
    let (_, raw_tx, expected_tx_hash) =
        create_signed_eip1559_tx(&signer, L2_CHAIN_ID, nonce, recipient)?;

    let current_block = builder_provider.get_block_number().await?;
    let target_block = current_block + 2;

    let request = BundleRequest {
        txs: vec![raw_tx],
        block_number: Some(target_block),
        min_timestamp: None,
        max_timestamp: None,
        reverting_tx_hashes: None,
        replacement_uuid: None,
        builders: None,
    };

    let rpc_client = RpcClient::builder().http(devnet.l2_client_rpc_url()?);
    let bundle_hash: B256 = rpc_client.request("eth_sendBundle", (request,)).await?;
    assert_eq!(bundle_hash, expected_tx_hash, "Bundle returned unexpected hash");

    wait_for_block(&builder_provider, target_block - 1).await?;
    let early_receipt = builder_provider.get_transaction_receipt(expected_tx_hash).await?;
    assert!(early_receipt.is_none(), "Bundle tx should not be included before target block");

    let receipt = timeout(TX_RECEIPT_TIMEOUT, async {
        loop {
            if let Some(receipt) =
                builder_provider.get_transaction_receipt(expected_tx_hash).await?
            {
                return Ok::<_, eyre::Error>(receipt);
            }
            sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .wrap_err("Transaction receipt timed out")?
    .wrap_err("Failed to get transaction receipt")?;

    assert_eq!(receipt.inner.block_number, Some(target_block));
    assert_eq!(receipt.inner.from, sender);
    assert_eq!(receipt.inner.to, Some(recipient));

    Ok(())
}

#[tokio::test]
async fn test_expired_bundle_is_not_included() -> Result<()> {
    let devnet = DevnetBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_tx_forwarding(TxForwardingConfig::new(vec![]))
        .build()
        .await?;

    let builder_provider = devnet.l2_builder_provider()?;
    let client_provider = devnet.l2_client_provider()?;

    wait_for_block(&builder_provider, 2).await?;
    wait_for_block(&client_provider, 2).await?;

    let private_key_hex = format!("0x{}", hex::encode(ANVIL_ACCOUNT_1.private_key.as_slice()));
    let signer: PrivateKeySigner = private_key_hex.parse()?;
    let sender = signer.address();

    wait_for_balance(&client_provider, sender).await?;

    let nonce = client_provider.get_transaction_count(sender).await?;
    let recipient: Address = "0x000000000000000000000000000000000000Cafe".parse()?;
    let (_, raw_tx, expected_tx_hash) =
        create_signed_eip1559_tx(&signer, L2_CHAIN_ID, nonce, recipient)?;

    let current_block = builder_provider.get_block_number().await?;
    let target_block = current_block + 2;
    let max_timestamp = unix_time_millis() as u64 + 500;

    let request = BundleRequest {
        txs: vec![raw_tx],
        block_number: Some(target_block),
        min_timestamp: None,
        max_timestamp: Some(max_timestamp),
        reverting_tx_hashes: None,
        replacement_uuid: None,
        builders: None,
    };

    let rpc_client = RpcClient::builder().http(devnet.l2_client_rpc_url()?);
    let bundle_hash: B256 = rpc_client.request("eth_sendBundle", (request,)).await?;
    assert_eq!(bundle_hash, expected_tx_hash, "Bundle returned unexpected hash");

    wait_for_block(&builder_provider, target_block + 1).await?;

    for _ in 0..6 {
        let receipt = builder_provider.get_transaction_receipt(expected_tx_hash).await?;
        assert!(receipt.is_none(), "Expired bundle tx should not be included");
        sleep(Duration::from_millis(500)).await;
    }

    Ok(())
}
