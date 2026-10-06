//! Local integration coverage for the snapshot-backed development network.

use std::time::Duration;

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::BlockNumberOrTag;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionRequest;
use base_system_tests::{
    ANVIL_ACCOUNT_1, DevnetBlockInterval, DevnetConfig, DevnetL2State, DevnetPrefund,
    SnapshotChainConfig, SnapshotL2Stack, SystemTestStackBuilder,
};
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const TX_TIMEOUT: Duration = Duration::from_secs(30);

/// Starts real EL and CL components from caller-owned writable Base snapshots.
#[tokio::test]
#[ignore = "requires two writable Base execution snapshot datadirs"]
async fn snapshot_devnet_mines_follows_and_includes_rpc_transaction() -> Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();
    let builder_datadir = std::env::var_os("BASE_SNAPSHOT_BUILDER_DATADIR")
        .expect("BASE_SNAPSHOT_BUILDER_DATADIR must be set");
    let client_datadir = std::env::var_os("BASE_SNAPSHOT_CLIENT_DATADIR")
        .expect("BASE_SNAPSHOT_CLIENT_DATADIR must be set");
    let chain = std::env::var("BASE_SNAPSHOT_CHAIN").unwrap_or_else(|_| "mainnet".to_string());
    let rollup_config = std::env::var_os("BASE_SNAPSHOT_ROLLUP_CONFIG").map(Into::into);
    let mut config = DevnetConfig::snapshot(
        builder_datadir.into(),
        client_datadir.into(),
        SnapshotChainConfig { chain, rollup_config },
    )?;
    let signer: PrivateKeySigner =
        format!("0x{}", hex::encode(ANVIL_ACCOUNT_1.private_key.as_slice())).parse()?;
    let DevnetL2State::Snapshot(snapshot) = &mut config.l2_state else {
        unreachable!("snapshot constructor must create snapshot state")
    };
    if let Some(value) = std::env::var_os("BASE_SNAPSHOT_BLOCK_INTERVAL") {
        snapshot.block_interval = match value.to_str() {
            Some("2s") => DevnetBlockInterval::TwoSeconds,
            Some("200ms") => DevnetBlockInterval::TwoHundredMilliseconds,
            _ => eyre::bail!("BASE_SNAPSHOT_BLOCK_INTERVAL must be 2s or 200ms"),
        };
    }
    snapshot.prefund =
        Some(DevnetPrefund { address: signer.address(), amount: 1_000_000_000_000_000_000 });
    config.validate()?;
    let stack = SystemTestStackBuilder::new().with_devnet_config(config).build_snapshot().await?;

    assert!(stack.boundary().head.number > 0);
    assert!(stack.boundary().l2_block_info.seq_num > 0);
    let current = stack.current_builder_boundary().await.expect("current head must decode");
    assert!(current.head.number >= stack.boundary().head.number + 2);
    assert_eq!(
        current.l2_block_info.seq_num,
        stack.boundary().l2_block_info.seq_num + current.head.number - stack.boundary().head.number
    );
    assert_block_interval(&stack).await?;
    send_transaction_and_wait_for_both(&stack, signer).await?;
    stack.shutdown().await?;
    Ok(())
}

async fn assert_block_interval(stack: &SnapshotL2Stack) -> Result<()> {
    let builder = RootProvider::<Base>::new_http(stack.builder_rpc_url()?);
    let first_number = stack.boundary().head.number + 1;
    let first = builder
        .get_block_by_number(BlockNumberOrTag::Number(first_number))
        .await?
        .ok_or_else(|| eyre::eyre!("first snapshot descendant is missing"))?;
    let second = builder
        .get_block_by_number(BlockNumberOrTag::Number(first_number + 1))
        .await?
        .ok_or_else(|| eyre::eyre!("second snapshot descendant is missing"))?;

    match stack.block_interval() {
        DevnetBlockInterval::TwoSeconds => {
            eyre::ensure!(first.header.timestamp_ms.is_none());
            eyre::ensure!(second.header.timestamp_ms.is_none());
            eyre::ensure!(second.header.timestamp == first.header.timestamp + 2);
        }
        DevnetBlockInterval::TwoHundredMilliseconds => {
            let first_timestamp = first
                .header
                .timestamp_ms
                .ok_or_else(|| eyre::eyre!("first subsecond timestamp is missing"))?;
            let second_timestamp = second
                .header
                .timestamp_ms
                .ok_or_else(|| eyre::eyre!("second subsecond timestamp is missing"))?;
            eyre::ensure!(second_timestamp == first_timestamp + 200);
        }
    }
    Ok(())
}

async fn send_transaction_and_wait_for_both(
    stack: &SnapshotL2Stack,
    signer: PrivateKeySigner,
) -> Result<()> {
    let builder = RootProvider::<Base>::new_http(stack.builder_rpc_url()?);
    let client = RootProvider::<Base>::new_http(stack.client_rpc_url()?);
    let sender = signer.address();
    let balance = builder.get_balance(sender).await?;
    eyre::ensure!(balance > U256::ZERO, "snapshot prefund was not applied");

    let transaction = BaseTransactionRequest::default()
        .from(sender)
        .to(Address::repeat_byte(0xde))
        .value(U256::from(1))
        .transaction_type(2)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(10_000_000_000)
        .with_max_priority_fee_per_gas(1_000_000)
        .with_chain_id(stack.chain_id())
        .with_nonce(builder.get_transaction_count(sender).await?)
        .build_typed_tx()
        .map_err(|_| eyre::eyre!("invalid snapshot test transaction"))?;
    let signature = signer.sign_hash_sync(&transaction.signature_hash())?;
    let signed = transaction.into_signed(signature);
    let hash = *signed.hash();
    let raw: Bytes = signed.encoded_2718().into();
    let pending = builder
        .send_raw_transaction(&raw)
        .await
        .wrap_err("failed to submit normal RPC transaction")?;
    eyre::ensure!(*pending.tx_hash() == hash, "submitted transaction hash changed");

    let builder_receipt = timeout(TX_TIMEOUT, async {
        loop {
            if let Some(receipt) = builder.get_transaction_receipt(hash).await? {
                return Ok::<_, eyre::Report>(receipt);
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .wrap_err("timed out waiting for transaction on builder")??;
    let client_receipt = timeout(TX_TIMEOUT, async {
        loop {
            if let Some(receipt) = client.get_transaction_receipt(hash).await?
                && receipt.inner.block_hash == builder_receipt.inner.block_hash
            {
                return Ok::<_, eyre::Report>(receipt);
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .wrap_err("timed out waiting for transaction on client")??;
    assert_eq!(builder_receipt.inner.block_hash, client_receipt.inner.block_hash);
    Ok(())
}
