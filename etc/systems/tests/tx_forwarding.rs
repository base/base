//! System tests for the transaction forwarding pipeline.
//!
//! These tests verify that transactions can be forwarded from mempool nodes
//! to builder nodes via the `base_insertValidatedTransaction` RPC endpoint.

use std::time::Duration;

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::Provider;
use alloy_rpc_client::RpcClient;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_rpc_types::BaseTransactionRequest;
use base_execution_txpool::{NoExtensions, ValidatedTransaction};
use base_system_tests::{
    ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_2, ANVIL_ACCOUNT_3, ANVIL_ACCOUNT_4, SystemTestProviderExt,
    SystemTestStack, SystemTestStackBuilder,
};
use base_tx_forwarding::TxForwardingConfig;
use base_txpool_rpc::SendRawTransactionValidityRequest;
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

/// Creates a signed EIP-1559 transaction and returns the sender, raw bytes, and tx hash.
fn create_signed_eip1559_tx(
    signer: &PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    recipient: Address,
) -> Result<(Address, Bytes, alloy_primitives::B256)> {
    let sender = signer.address();

    let tx_request = BaseTransactionRequest::default()
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

const DEAD: &str = "0x000000000000000000000000000000000000dEaD";
/// Reth's Prometheus recorder prefixes every key with `reth.`.
const INLINE_SIM_SECONDS: &str = "reth_inline_simulation_sim_seconds_count";
const INLINE_SIM_DEFAULTS: &str = "reth_inline_simulation_sim_default_inserts";
const INLINE_SIM_QUEUE_FULL: &str = "reth_inline_simulation_sim_queue_full";
const INLINE_SIM_FAILURES: &str = "reth_inline_simulation_sim_failures";

/// Process-global Prometheus counters; serialize the scrapers.
static INLINE_SIM_E2E: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Boots L1+L2 with forwarding, then waits until both EL nodes have produced a few blocks.
async fn boot_forwarding_stack(forwarding: TxForwardingConfig) -> Result<SystemTestStack> {
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_tx_forwarding(forwarding)
        .build()
        .await?;
    let builder = system.l2_builder_provider()?;
    let client = system.l2_client_provider()?;
    builder.wait_for_block(3, Duration::from_secs(15)).await?;
    client.wait_for_block(3, Duration::from_secs(15)).await?;
    Ok(system)
}

/// Signs with an Anvil genesis account.
fn anvil_signer(private_key: &[u8]) -> Result<PrivateKeySigner> {
    let hex = format!("0x{}", hex::encode(private_key));
    hex.parse().map_err(|e| eyre::eyre!("invalid private key: {e:?}"))
}

/// Parses a Prometheus counter/gauge/histogram `_count` line. Missing series is 0.
fn prometheus_value(body: &str, name: &str, label: Option<&str>) -> f64 {
    let mut total = 0.0;
    let mut found = false;
    for line in body.lines() {
        if line.starts_with('#') {
            continue;
        }
        let Some((metric, value)) = line.rsplit_once(' ') else {
            continue;
        };
        let (base, labels) = match metric.split_once('{') {
            Some((base, rest)) => (base, Some(rest.trim_end_matches('}'))),
            None => (metric, None),
        };
        if base != name {
            continue;
        }
        if let Some(want) = label {
            if !labels.is_some_and(|got| got.contains(want)) {
                continue;
            }
        }
        if let Ok(v) = value.parse::<f64>() {
            total += v;
            found = true;
        }
    }
    if found { total } else { 0.0 }
}

/// Scrapes the client's Prometheus exporter, retrying until it accepts connections.
async fn scrape_metrics(url: &url::Url) -> Result<String> {
    let mut last_err = None;
    for _ in 0..30 {
        match reqwest::get(url.clone()).await {
            Ok(resp) => {
                return resp.text().await.wrap_err("metrics body");
            }
            Err(err) => {
                last_err = Some(err);
                sleep(Duration::from_millis(100)).await;
            }
        }
    }
    Err(eyre::eyre!("metrics scrape failed: {last_err:?}"))
}

/// Polls until `name` is at least `before + min_delta`.
async fn wait_metric_delta(
    url: &url::Url,
    name: &str,
    label: Option<&str>,
    before: f64,
    min_delta: f64,
) -> Result<f64> {
    match timeout(Duration::from_secs(10), async {
        loop {
            let body = scrape_metrics(url).await?;
            let now = prometheus_value(&body, name, label);
            if now - before >= min_delta {
                return Ok::<_, eyre::Error>(now);
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    {
        Ok(inner) => inner,
        Err(_) => {
            let body = scrape_metrics(url).await.unwrap_or_default();
            let sample: String = body
                .lines()
                .filter(|line| line.contains("inline_simulation") || line.contains("sim_"))
                .take(40)
                .collect::<Vec<_>>()
                .join("\n");
            eyre::bail!(
                "{name} did not increase (before={before}). matching scrape lines:\n{sample}"
            )
        }
    }
}

/// Tests that a single transaction can be inserted via `base_insertValidatedTransaction`.
///
/// This is the foundational test for the forwarding pipeline. It verifies:
/// 1. The builder node has the `base_insertValidatedTransaction` RPC endpoint
/// 2. The endpoint accepts a valid pre-validated transaction
/// 3. The transaction is included in a block on the builder
#[tokio::test]
async fn test_insert_validated_transaction_single() -> Result<()> {
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .build()
        .await?;

    let builder_provider = system.l2_builder_provider()?;

    // Wait for some blocks to be produced so the chain is ready
    timeout(Duration::from_secs(15), async {
        loop {
            let block = builder_provider.get_block_number().await?;
            if block >= 2 {
                return Ok::<_, eyre::Error>(block);
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .wrap_err("Builder block production timed out")??;

    // Set up the signer with a funded account
    let private_key_hex = format!("0x{}", hex::encode(ANVIL_ACCOUNT_1.private_key.as_slice()));
    let signer: PrivateKeySigner = private_key_hex.parse()?;
    let sender = signer.address();

    // Verify sender has balance
    let balance = builder_provider.get_balance(sender).await?;
    assert!(balance > U256::ZERO, "Sender should have balance");

    // Get current nonce
    let nonce = builder_provider.get_transaction_count(sender).await?;

    // Create a signed transaction
    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;
    let (sender, raw_tx, expected_tx_hash) =
        create_signed_eip1559_tx(&signer, L2_CHAIN_ID, nonce, recipient)?;

    // Create the ValidatedTransaction payload
    let validated_tx = ValidatedTransaction {
        sender,
        raw: raw_tx,
        min_block_number: None,
        max_block_number: None,
        min_timestamp: None,
        max_timestamp: None,
        metering: None,
        extensions: NoExtensions {},
    };

    // Create RPC client for the builder
    let builder_rpc_url = system.l2_rpc_url()?;
    let rpc_client = RpcClient::builder().http(builder_rpc_url);

    // Call base_insertValidatedTransaction
    let result: Result<(), _> =
        rpc_client.request("base_insertValidatedTransaction", (validated_tx,)).await;

    assert!(result.is_ok(), "base_insertValidatedTransaction should succeed, got: {result:?}");

    // Wait for the transaction to be included in a block
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

    // Verify the transaction was included
    assert_eq!(receipt.inner.transaction_hash, expected_tx_hash);
    assert!(receipt.inner.block_number.is_some(), "Receipt should have block number");
    assert_eq!(receipt.inner.from, sender);
    assert_eq!(receipt.inner.to, Some(recipient));

    Ok(())
}

/// Full system test for the transaction forwarding pipeline.
///
/// This test verifies the complete flow:
/// 1. Client node receives a transaction
/// 2. `TxForwardingExtension` picks it up from the mempool
/// 3. Forwarder calls `base_insertValidatedTransaction` on the builder
/// 4. Transaction is included in a block on the builder
///
/// This is different from `test_insert_validated_transaction_single` which
/// directly calls the RPC endpoint. Here we test the full pipeline.
#[tokio::test]
async fn test_tx_forwarding_pipeline_system() -> Result<()> {
    // Build a system test stack with tx forwarding enabled on the client.
    // The client will forward transactions to the builder's RPC endpoint
    let forwarding =
        TxForwardingConfig::new(vec![]).with_resend_after_ms(2000).with_max_batch_size(100);
    assert!(
        !forwarding.inline_simulation,
        "inline simulation must stay off so this path still inserts then forwards"
    );

    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_tx_forwarding(forwarding)
        .build()
        .await?;

    let builder_provider = system.l2_builder_provider()?;
    let client_provider = system.l2_client_provider()?;

    // Wait for some blocks to be produced so both nodes are synced
    timeout(Duration::from_secs(15), async {
        loop {
            let builder_block = builder_provider.get_block_number().await?;
            let client_block = client_provider.get_block_number().await?;
            if builder_block >= 3 && client_block >= 3 {
                return Ok::<_, eyre::Error>((builder_block, client_block));
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .wrap_err("Block production/sync timed out")??;

    // Set up the signer with a funded account
    let private_key_hex = format!("0x{}", hex::encode(ANVIL_ACCOUNT_1.private_key.as_slice()));
    let signer: PrivateKeySigner = private_key_hex.parse()?;
    let sender = signer.address();

    // Wait for client to sync balance
    timeout(Duration::from_secs(15), async {
        loop {
            let client_balance = client_provider.get_balance(sender).await?;
            if client_balance > U256::ZERO {
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .wrap_err("Timed out waiting for client to sync balance")??;

    // Get nonce from client (the node we'll send to)
    let nonce = client_provider.get_transaction_count(sender).await?;

    // Create a signed transaction
    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;
    let (_, raw_tx, expected_tx_hash) =
        create_signed_eip1559_tx(&signer, L2_CHAIN_ID, nonce, recipient)?;

    // Send the transaction to the CLIENT node (not builder)
    // The forwarding pipeline should forward it to the builder
    let pending_tx = client_provider
        .send_raw_transaction(&raw_tx)
        .await
        .wrap_err("Failed to send transaction to client")?;
    let tx_hash = *pending_tx.tx_hash();
    assert_eq!(tx_hash, expected_tx_hash, "Transaction hash mismatch");

    // Wait for the transaction to be included in a block on the BUILDER
    // This proves the forwarding pipeline worked
    let receipt = timeout(TX_RECEIPT_TIMEOUT, async {
        loop {
            if let Some(receipt) =
                builder_provider.get_transaction_receipt(expected_tx_hash).await?
            {
                return Ok::<_, eyre::Error>(receipt);
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .wrap_err("Transaction receipt timed out on builder - forwarding may have failed")?
    .wrap_err("Failed to get transaction receipt")?;

    // Verify the transaction was included
    assert_eq!(receipt.inner.transaction_hash, expected_tx_hash);
    assert!(receipt.inner.block_number.is_some(), "Receipt should have block number");
    assert_eq!(receipt.inner.from, sender);
    assert_eq!(receipt.inner.to, Some(recipient));

    Ok(())
}

/// Same pipeline as [`test_tx_forwarding_pipeline_system`] with inline simulation on.
///
/// `sendRaw` on the client waits for meter_bundle plus pool insert, then the
/// forwarder sends the tx to the builder. Scrapes Prometheus so this is not just
/// the OG insert path.
#[tokio::test]
async fn test_tx_forwarding_pipeline_with_inline_simulation() -> Result<()> {
    let _guard = INLINE_SIM_E2E.lock().await;
    let forwarding = TxForwardingConfig::new(vec![])
        .with_resend_after_ms(2000)
        .with_max_batch_size(100)
        .with_inline_simulation(true);

    let system = boot_forwarding_stack(forwarding).await?;
    let builder_provider = system.l2_builder_provider()?;
    let client_provider = system.l2_client_provider()?;
    let metrics = system.l2_stack().client().metrics_url()?;

    let signer = anvil_signer(ANVIL_ACCOUNT_1.private_key.as_slice())?;
    let sender = signer.address();
    client_provider.wait_for_balance(sender, Duration::from_secs(15)).await?;

    let before = scrape_metrics(&metrics).await?;
    let seconds_before =
        prometheus_value(&before, INLINE_SIM_SECONDS, None);
    let defaults_before =
        prometheus_value(&before, INLINE_SIM_DEFAULTS, None);

    let nonce = client_provider.get_transaction_count(sender).await?;
    let recipient: Address = DEAD.parse()?;
    let (_, raw_tx, expected_tx_hash) =
        create_signed_eip1559_tx(&signer, L2_CHAIN_ID, nonce, recipient)?;

    let pending_tx = client_provider
        .send_raw_transaction(&raw_tx)
        .await
        .wrap_err("Failed to send transaction to client")?;
    assert_eq!(*pending_tx.tx_hash(), expected_tx_hash, "Transaction hash mismatch");

    let seconds_after = wait_metric_delta(
        &metrics,
        INLINE_SIM_SECONDS,
        None,
        seconds_before,
        1.0,
    )
    .await?;
    assert!(
        seconds_after - seconds_before >= 1.0,
        "meter_bundle must run; sim_seconds_count {seconds_before} -> {seconds_after}"
    );

    let after = scrape_metrics(&metrics).await?;
    let defaults_after =
        prometheus_value(&after, INLINE_SIM_DEFAULTS, None);
    assert_eq!(
        defaults_after, defaults_before,
        "this tx must insert real metering, not MeterBundleResponse::default"
    );

    let receipt = builder_provider.wait_for_receipt(expected_tx_hash, TX_RECEIPT_TIMEOUT).await?;
    assert_eq!(receipt.inner.transaction_hash, expected_tx_hash);
    assert!(receipt.inner.block_number.is_some(), "Receipt should have block number");
    assert_eq!(receipt.inner.from, sender);
    assert_eq!(receipt.inner.to, Some(recipient));

    Ok(())
}

/// Flag on: cheap validate still rejects a tx that cannot pay, without waiting on the oneshot.
#[tokio::test]
async fn test_inline_simulation_rejects_unfunded_sender() -> Result<()> {
    let _guard = INLINE_SIM_E2E.lock().await;
    let forwarding = TxForwardingConfig::new(vec![])
        .with_resend_after_ms(2000)
        .with_max_batch_size(100)
        .with_inline_simulation(true);

    let system = boot_forwarding_stack(forwarding).await?;
    let client_provider = system.l2_client_provider()?;
    let signer = PrivateKeySigner::random();
    let recipient: Address = DEAD.parse()?;
    let (_, raw_tx, _) = create_signed_eip1559_tx(&signer, L2_CHAIN_ID, 0, recipient)?;

    let err = timeout(Duration::from_secs(15), client_provider.send_raw_transaction(&raw_tx))
        .await
        .wrap_err("unfunded sendRaw hung on the inline-sim oneshot")?
        .expect_err("unfunded sender must fail cheap validate");
    assert!(
        !err.to_string().is_empty(),
        "RPC error should describe the rejected transaction"
    );

    Ok(())
}

/// Capacity 1 + one worker: concurrent sendRaws from four senders overflow the queue.
/// Full falls back to unmetered insert; hashes still return and txs still land.
#[tokio::test]
async fn test_inline_simulation_queue_full_still_forwards() -> Result<()> {
    let _guard = INLINE_SIM_E2E.lock().await;
    let forwarding = TxForwardingConfig::new(vec![])
        .with_resend_after_ms(2000)
        .with_max_batch_size(100)
        .with_inline_simulation(true)
        .with_inline_simulation_workers(1)
        .with_inline_simulation_queue_capacity(1);

    let system = boot_forwarding_stack(forwarding).await?;
    let builder_provider = system.l2_builder_provider()?;
    let client_provider = system.l2_client_provider()?;
    let metrics = system.l2_stack().client().metrics_url()?;
    let recipient: Address = DEAD.parse()?;

    let accounts = [&*ANVIL_ACCOUNT_1, &*ANVIL_ACCOUNT_2, &*ANVIL_ACCOUNT_3, &*ANVIL_ACCOUNT_4];
    let mut raws = Vec::new();
    for account in accounts {
        let signer = anvil_signer(account.private_key.as_slice())?;
        client_provider.wait_for_balance(signer.address(), Duration::from_secs(15)).await?;
        let nonce = client_provider.get_transaction_count(signer.address()).await?;
        let (_, raw_tx, hash) =
            create_signed_eip1559_tx(&signer, L2_CHAIN_ID, nonce, recipient)?;
        raws.push((signer.address(), raw_tx, hash));
    }

    let before = scrape_metrics(&metrics).await?;
    let full_before = prometheus_value(&before, INLINE_SIM_QUEUE_FULL, None);

    let (r0, r1, r2, r3) = tokio::join!(
        client_provider.send_raw_transaction(&raws[0].1),
        client_provider.send_raw_transaction(&raws[1].1),
        client_provider.send_raw_transaction(&raws[2].1),
        client_provider.send_raw_transaction(&raws[3].1),
    );
    for (i, result) in [r0, r1, r2, r3].into_iter().enumerate() {
        let pending = result.wrap_err_with(|| format!("sendRaw {i} failed"))?;
        assert_eq!(*pending.tx_hash(), raws[i].2, "hash mismatch for sendRaw {i}");
    }

    let full_after = wait_metric_delta(
        &metrics,
        INLINE_SIM_QUEUE_FULL,
        None,
        full_before,
        1.0,
    )
    .await?;
    assert!(
        full_after - full_before >= 1.0,
        "at least one concurrent sendRaw must overflow the 1-slot queue; {full_before} -> {full_after}"
    );

    for (sender, _, hash) in &raws {
        let receipt = builder_provider.wait_for_receipt(*hash, TX_RECEIPT_TIMEOUT).await?;
        assert_eq!(receipt.inner.transaction_hash, *hash);
        assert_eq!(receipt.inner.from, *sender);
        assert_eq!(receipt.inner.to, Some(recipient));
    }

    Ok(())
}

/// Timeout 0ms: sleep(0) wins vs spawn_blocking, so the worker inserts Default metering.
/// The tx still returns a hash and still lands on the builder.
#[tokio::test]
async fn test_inline_simulation_timeout_still_forwards() -> Result<()> {
    let _guard = INLINE_SIM_E2E.lock().await;
    let forwarding = TxForwardingConfig::new(vec![])
        .with_resend_after_ms(2000)
        .with_max_batch_size(100)
        .with_inline_simulation(true)
        .with_inline_simulation_timeout_ms(0);

    let system = boot_forwarding_stack(forwarding).await?;
    let builder_provider = system.l2_builder_provider()?;
    let client_provider = system.l2_client_provider()?;
    let metrics = system.l2_stack().client().metrics_url()?;

    let signer = anvil_signer(ANVIL_ACCOUNT_1.private_key.as_slice())?;
    let sender = signer.address();
    client_provider.wait_for_balance(sender, Duration::from_secs(15)).await?;

    let before = scrape_metrics(&metrics).await?;
    let timeout_before = prometheus_value(
        &before,
        INLINE_SIM_FAILURES,
        Some(r#"reason="timeout""#),
    );
    let defaults_before =
        prometheus_value(&before, INLINE_SIM_DEFAULTS, None);

    let nonce = client_provider.get_transaction_count(sender).await?;
    let recipient: Address = DEAD.parse()?;
    let (_, raw_tx, expected_tx_hash) =
        create_signed_eip1559_tx(&signer, L2_CHAIN_ID, nonce, recipient)?;

    let pending_tx = client_provider
        .send_raw_transaction(&raw_tx)
        .await
        .wrap_err("Failed to send transaction to client")?;
    assert_eq!(*pending_tx.tx_hash(), expected_tx_hash, "Transaction hash mismatch");

    let timeout_after = wait_metric_delta(
        &metrics,
        INLINE_SIM_FAILURES,
        Some(r#"reason="timeout""#),
        timeout_before,
        1.0,
    )
    .await?;
    assert!(
        timeout_after - timeout_before >= 1.0,
        "timeout=0 must count a timeout failure; {timeout_before} -> {timeout_after}"
    );

    let after = scrape_metrics(&metrics).await?;
    let defaults_after =
        prometheus_value(&after, INLINE_SIM_DEFAULTS, None);
    assert!(
        defaults_after - defaults_before >= 1.0,
        "timed-out sim must insert MeterBundleResponse::default; {defaults_before} -> {defaults_after}"
    );

    let receipt = builder_provider.wait_for_receipt(expected_tx_hash, TX_RECEIPT_TIMEOUT).await?;
    assert_eq!(receipt.inner.transaction_hash, expected_tx_hash);
    assert!(receipt.inner.block_number.is_some(), "Receipt should have block number");
    assert_eq!(receipt.inner.from, sender);
    assert_eq!(receipt.inner.to, Some(recipient));

    Ok(())
}

/// Tests validity-bearing transaction ingress through the production forwarding pipeline.
#[tokio::test]
async fn test_validity_tx_forwarding_pipeline_system() -> Result<()> {
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_tx_forwarding(
            TxForwardingConfig::new(vec![]).with_resend_after_ms(2000).with_max_batch_size(100),
        )
        .with_experimental_validity_transactions()
        .build()
        .await?;

    let builder_provider = system.l2_builder_provider()?;
    let client_provider = system.l2_client_provider()?;
    builder_provider.wait_for_block(3, Duration::from_secs(15)).await?;
    client_provider.wait_for_block(3, Duration::from_secs(15)).await?;

    let private_key_hex = format!("0x{}", hex::encode(ANVIL_ACCOUNT_1.private_key.as_slice()));
    let signer: PrivateKeySigner = private_key_hex.parse()?;
    let sender = signer.address();
    client_provider.wait_for_balance(sender, Duration::from_secs(15)).await?;

    let nonce = client_provider.get_transaction_count(sender).await?;
    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;
    let (_, raw_tx, expected_tx_hash) =
        create_signed_eip1559_tx(&signer, L2_CHAIN_ID, nonce, recipient)?;
    let validity = serde_json::from_value(serde_json::json!({
        "type": "storage",
        "params": {
            "address": recipient,
            "slot": "0x1",
            "op": "=",
            "value": "0x2"
        }
    }))?;
    let rpc_client = RpcClient::builder().http(system.l2_client_rpc_url()?);

    let tx_hash: alloy_primitives::B256 = rpc_client
        .request(
            "base_sendRawTransactionValidity",
            (SendRawTransactionValidityRequest { tx: raw_tx, validity: vec![validity] },),
        )
        .await?;

    assert_eq!(tx_hash, expected_tx_hash, "Transaction hash mismatch");

    // TODO: Update this "transaction landed" assertion when validity transactions are split out
    // of the builder's regular txpool; the dedicated validity pool will need its own observable.
    let receipt = builder_provider.wait_for_receipt(expected_tx_hash, TX_RECEIPT_TIMEOUT).await?;
    assert_eq!(receipt.inner.transaction_hash, expected_tx_hash);
    assert!(receipt.inner.block_number.is_some(), "Receipt should have block number");
    assert_eq!(receipt.inner.from, sender);
    assert_eq!(receipt.inner.to, Some(recipient));

    Ok(())
}

/// Tests that the forwarding pipeline handles high transaction load under rate limiting.
///
/// Uses all 4 available test accounts (`ANVIL_ACCOUNT_1` through `ANVIL_ACCOUNT_4`) to send
/// transactions concurrently, with `max_rps = 1` forcing the forwarder to buffer heavily.
/// Each account sends 10 transactions for a total of 40, verifying that all are eventually
/// forwarded to the builder and included in blocks despite the constrained send rate.
#[tokio::test]
async fn test_tx_forwarding_pipeline_system_high_load() -> Result<()> {
    const TXS_PER_ACCOUNT: usize = 10;

    let accounts = [&*ANVIL_ACCOUNT_1, &*ANVIL_ACCOUNT_2, &*ANVIL_ACCOUNT_3, &*ANVIL_ACCOUNT_4];

    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_tx_forwarding(
            TxForwardingConfig::new(vec![]).with_max_rps(1).with_resend_after_ms(30_000), // high resend window so we don't double-send
        )
        .build()
        .await?;

    let builder_provider = system.l2_builder_provider()?;
    let client_provider = system.l2_client_provider()?;

    // Wait for some blocks to be produced so both nodes are synced
    timeout(Duration::from_secs(15), async {
        loop {
            let builder_block = builder_provider.get_block_number().await?;
            let client_block = client_provider.get_block_number().await?;
            if builder_block >= 3 && client_block >= 3 {
                return Ok::<_, eyre::Error>((builder_block, client_block));
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .wrap_err("Block production/sync timed out")??;

    // Set up signers for all accounts
    let signers: Vec<PrivateKeySigner> = accounts
        .iter()
        .map(|acct| {
            let hex = format!("0x{}", hex::encode(acct.private_key.as_slice()));
            hex.parse::<PrivateKeySigner>().map_err(|e| eyre::eyre!("invalid private key: {e:?}"))
        })
        .collect::<Result<Vec<PrivateKeySigner>>>()?;

    // Wait for all accounts to have balance on the client
    timeout(Duration::from_secs(15), async {
        loop {
            let mut all_funded = true;
            for signer in &signers {
                let balance = client_provider.get_balance(signer.address()).await?;
                if balance == U256::ZERO {
                    all_funded = false;
                    break;
                }
            }
            if all_funded {
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .wrap_err("Timed out waiting for all accounts to sync balance")??;

    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;

    // Send TXS_PER_ACCOUNT transactions from each signer, interleaving accounts
    // to maximize concurrency pressure on the forwarder
    let mut expected: Vec<(Address, alloy_primitives::B256)> = Vec::new();
    let mut nonces: Vec<u64> = Vec::with_capacity(signers.len());
    for signer in &signers {
        let nonce = client_provider.get_transaction_count(signer.address()).await?;
        nonces.push(nonce);
    }

    for tx_idx in 0..TXS_PER_ACCOUNT {
        for (acct_idx, signer) in signers.iter().enumerate() {
            let nonce = nonces[acct_idx] + tx_idx as u64;
            let (_, raw_tx, expected_tx_hash) =
                create_signed_eip1559_tx(signer, L2_CHAIN_ID, nonce, recipient)?;

            let pending_tx = client_provider
                .send_raw_transaction(&raw_tx)
                .await
                .wrap_err_with(|| format!("Failed to send tx {tx_idx} from account {acct_idx}"))?;

            assert_eq!(
                *pending_tx.tx_hash(),
                expected_tx_hash,
                "Transaction hash mismatch for tx {tx_idx} from account {acct_idx}"
            );
            expected.push((signer.address(), expected_tx_hash));
        }
    }

    let total_txs = expected.len();

    // Wait for ALL transactions to be included on the builder.
    // With max_rps=1 and 40 txs, the forwarder needs significant time to drain.
    let mut received = vec![false; total_txs];

    timeout(Duration::from_secs(180), async {
        loop {
            let mut all_received = true;
            for (i, (sender, hash)) in expected.iter().enumerate() {
                if received[i] {
                    continue;
                }
                if let Some(receipt) = builder_provider.get_transaction_receipt(*hash).await? {
                    assert_eq!(receipt.inner.transaction_hash, *hash);
                    assert_eq!(receipt.inner.from, *sender);
                    assert_eq!(receipt.inner.to, Some(recipient));
                    received[i] = true;
                } else {
                    all_received = false;
                }
            }
            if all_received {
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .wrap_err("Timed out waiting for all transactions - forwarding under load may have failed")??;

    let included_count = received.iter().filter(|&&r| r).count();
    assert_eq!(
        included_count, total_txs,
        "Expected all {total_txs} transactions to be included, got {included_count}"
    );

    Ok(())
}
