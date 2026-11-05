//! End-to-end tests for the Denim network upgrade on a fresh devnet: the native payload builder
//! includes pool transactions before and after Denim activation, with and without a later Zenith,
//! and the chain switches from 2s to 200ms blocks at Denim.

use std::time::Duration;

use alloy_consensus::SignableTransaction;
use alloy_eips::{BlockNumberOrTag, eip2718::Encodable2718};
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_genesis::{BaseUpgrade, RollupConfig};
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionRequest;
use base_system_tests::{ANVIL_ACCOUNT_1, SystemTestStackBuilder};
use eyre::{OptionExt, Result, WrapErr};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
// Leave enough pre-Denim runway for both stacks to start under the system-test concurrency used
// in CI. A very early activation makes the pre-Denim transaction race node startup.
const DENIM_ACTIVATION_BLOCK: u64 = 25;
const BLOCK_TIMEOUT: Duration = Duration::from_secs(45);

#[tokio::test]
async fn native_builder_includes_transactions_across_denim() -> Result<()> {
    verify_denim_activation(None).await
}

#[tokio::test]
async fn native_builder_includes_transactions_across_denim_and_zenith() -> Result<()> {
    verify_denim_activation(Some(DENIM_ACTIVATION_BLOCK + 10)).await
}

async fn verify_denim_activation(zenith_activation_block: Option<u64>) -> Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();

    let mut setup = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_base_cobalt_activation_block(0)
        .with_base_denim_activation_block(DENIM_ACTIVATION_BLOCK);
    if let Some(block) = zenith_activation_block {
        setup = setup.with_base_zenith_activation_block(block);
    }
    let system = setup.build().await?;
    let builder = system.l2_builder_provider()?;
    let client = system.l2_client_provider()?;
    let signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)?;
    let rollup_config: RollupConfig =
        serde_json::from_slice(&std::fs::read(system.l2_deployment().rollup_config_path())?)?;
    let zenith_timestamp = rollup_config.upgrade_activation_timestamp(BaseUpgrade::Zenith);
    assert_eq!(zenith_timestamp.is_some(), zenith_activation_block.is_some());

    let pre_denim_receipt_block =
        send_transaction(&builder, &signer).await.wrap_err("pre-Denim transaction failed")?;
    assert!(
        pre_denim_receipt_block < DENIM_ACTIVATION_BLOCK,
        "pre-Denim transaction landed at block {pre_denim_receipt_block}"
    );

    wait_for_block(&builder, DENIM_ACTIVATION_BLOCK + 1).await?;
    let post_denim_receipt_block =
        send_transaction(&builder, &signer).await.wrap_err("post-Denim transaction failed")?;
    assert!(
        post_denim_receipt_block > DENIM_ACTIVATION_BLOCK,
        "post-Denim transaction landed at block {post_denim_receipt_block}"
    );

    let mut last_verified_block = post_denim_receipt_block + 4;
    if let Some(timestamp) = zenith_timestamp {
        // Use the generated schedule rather than assuming the harness's requested block number
        // still identifies the activation after Denim changes the block cadence.
        let zenith_active_head = wait_for_timestamp(&builder, timestamp).await?;
        let post_zenith_receipt_block =
            send_transaction(&builder, &signer).await.wrap_err("post-Zenith transaction failed")?;
        assert!(
            post_zenith_receipt_block > zenith_active_head,
            "post-Zenith transaction landed at block {post_zenith_receipt_block}"
        );
        last_verified_block = post_zenith_receipt_block + 4;
    }

    wait_for_block(&builder, last_verified_block).await?;
    wait_for_block(&client, last_verified_block).await?;
    verify_chain_and_cadence(&builder, &client, last_verified_block).await?;

    // Every pool transaction sent above is canonical and was composed by the native builder.
    // Pre-Denim payload jobs may rebuild a block before it is sealed, and each build records its
    // inclusion, so the count is a lower bound. The metrics recorder is process-global, so this
    // relies on nextest running each test in its own process.
    let metrics = reqwest::get(system.l2_stack().builder().metrics_url()?).await?.text().await?;
    let sent = if zenith_timestamp.is_some() { 3.0 } else { 2.0 };
    let standard_txs = r#"reth_base_builder_txs_included_per_block_sum{flow="standard"}"#;
    let included = metric_value(&metrics, standard_txs)?;
    assert!(included >= sent, "native builder recorded {included} inclusions for {sent} sent");
    for series in [
        "reth_base_builder_native_payload_build_duration_sum",
        "reth_base_builder_native_payload_execution_duration_sum",
        "reth_base_builder_native_payload_state_root_duration_sum",
    ] {
        assert!(metric_value(&metrics, series)? > 0.0, "native builder recorded no {series}");
    }

    Ok(())
}

fn metric_value(metrics: &str, series: &str) -> Result<f64> {
    metrics
        .lines()
        .find_map(|line| line.strip_prefix(series)?.strip_prefix(' '))
        .ok_or_else(|| eyre::eyre!("missing metric {series}"))?
        .parse()
        .wrap_err_with(|| format!("invalid metric {series}"))
}

async fn send_transaction(provider: &RootProvider<Base>, signer: &PrivateKeySigner) -> Result<u64> {
    let nonce = provider.get_transaction_count(signer.address()).await?;
    let transaction = BaseTransactionRequest::default()
        .from(signer.address())
        .to(Address::repeat_byte(0xfe))
        .value(U256::from(1))
        .transaction_type(2)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(2_000_000_000)
        .with_max_priority_fee_per_gas(1_000_000)
        .with_chain_id(L2_CHAIN_ID)
        .with_nonce(nonce)
        .build_typed_tx()
        .map_err(|error| eyre::eyre!("invalid transaction: {error:?}"))?;
    let signature = signer.sign_hash_sync(&transaction.signature_hash())?;
    let raw_transaction: Bytes = transaction.into_signed(signature).encoded_2718().into();
    let pending = provider.send_raw_transaction(&raw_transaction).await?;
    let transaction_hash = *pending.tx_hash();
    drop(pending);

    timeout(BLOCK_TIMEOUT, async {
        loop {
            if let Some(receipt) = provider.get_transaction_receipt(transaction_hash).await? {
                return receipt.inner.block_number.ok_or_eyre("receipt missing block number");
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .wrap_err("transaction receipt timed out")?
}

async fn wait_for_block(provider: &RootProvider<Base>, target: u64) -> Result<()> {
    timeout(BLOCK_TIMEOUT, async {
        while provider.get_block_number().await? < target {
            sleep(Duration::from_millis(100)).await;
        }
        Ok::<_, eyre::Error>(())
    })
    .await
    .wrap_err_with(|| format!("timed out waiting for block {target}"))?
}

async fn wait_for_timestamp(provider: &RootProvider<Base>, timestamp: u64) -> Result<u64> {
    timeout(BLOCK_TIMEOUT, async {
        loop {
            let block = provider
                .get_block_by_number(BlockNumberOrTag::Latest)
                .await?
                .ok_or_eyre("builder block missing")?;
            if block.header.timestamp >= timestamp {
                return Ok(block.header.number);
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .wrap_err_with(|| format!("timed out waiting for timestamp {timestamp}"))?
}

async fn verify_chain_and_cadence(
    builder: &RootProvider<Base>,
    client: &RootProvider<Base>,
    last_verified_block: u64,
) -> Result<()> {
    let mut previous_hash = None;
    let mut previous_timestamp_ms = None;

    for number in 0..=last_verified_block {
        let builder_block = builder
            .get_block_by_number(BlockNumberOrTag::Number(number))
            .await?
            .ok_or_eyre("builder block missing")?;
        let client_block = client
            .get_block_by_number(BlockNumberOrTag::Number(number))
            .await?
            .ok_or_eyre("client block missing")?;
        assert_eq!(builder_block.header.hash, client_block.header.hash, "block {number} mismatch");

        if let Some(hash) = previous_hash {
            assert_eq!(builder_block.header.parent_hash, hash, "non-contiguous block {number}");
        }

        let timestamp_ms = builder_block
            .header
            .timestamp_ms
            .unwrap_or_else(|| builder_block.header.timestamp.saturating_mul(1_000));
        if let Some(previous) = previous_timestamp_ms {
            let expected_delta = if number <= DENIM_ACTIVATION_BLOCK { 2_000 } else { 200 };
            assert_eq!(timestamp_ms - previous, expected_delta, "block {number} cadence mismatch");
        }

        previous_hash = Some(builder_block.header.hash);
        previous_timestamp_ms = Some(timestamp_ms);
    }

    Ok(())
}
