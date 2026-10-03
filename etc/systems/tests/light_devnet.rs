//! In-process coverage for the L1-free fresh-genesis light development network.
//!
//! Unlike the Docker-backed system tests, this starts only a builder and the standalone
//! sequencer in the test process, so it needs neither Docker nor an L1.

use std::time::Duration;

use alloy_consensus::{SignableTransaction, TxReceipt};
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
    ANVIL_ACCOUNT_1, DevnetBlockInterval, DevnetPrefund, InProcessNodeRuntime, LightL2Stack,
    LightL2StackConfig, SystemTestProviderExt,
};
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const BLOCK_TIMEOUT: Duration = Duration::from_secs(30);
const TX_TIMEOUT: Duration = Duration::from_secs(30);
const TRANSFER_WEI: u64 = 12_345;

/// Starts a light devnet, lets it advance, and includes a signed transfer.
async fn run_light_devnet(
    block_interval: DevnetBlockInterval,
    prefund: Option<DevnetPrefund>,
) -> Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();
    let stack = LightL2Stack::start(LightL2StackConfig {
        runtime: InProcessNodeRuntime::SystemTest,
        block_interval,
        prefund,
        ..Default::default()
    })
    .await?;
    let provider = RootProvider::<Base>::new_http(stack.builder_rpc_url()?);

    assert_eq!(provider.get_chain_id().await?, stack.chain_id());
    assert_eq!(
        provider.get_block_by_number(BlockNumberOrTag::Number(0)).await?.unwrap().header.hash,
        stack.genesis_hash()
    );
    let start = provider.get_block_number().await?;
    provider.wait_for_block(start + 3, BLOCK_TIMEOUT).await?;
    assert_block_interval(&provider, block_interval, start + 1).await?;
    send_transfer_and_wait(&provider, stack.chain_id()).await?;

    if let Some(prefund) = prefund {
        assert_eq!(provider.get_balance(prefund.address).await?, U256::from(prefund.amount));
    }
    stack.shutdown().await
}

/// Verifies that consecutive blocks follow the configured cadence.
async fn assert_block_interval(
    provider: &RootProvider<Base>,
    block_interval: DevnetBlockInterval,
    first_number: u64,
) -> Result<()> {
    let first = provider
        .get_block_by_number(BlockNumberOrTag::Number(first_number))
        .await?
        .ok_or_else(|| eyre::eyre!("first block {first_number} is missing"))?;
    let second = provider
        .get_block_by_number(BlockNumberOrTag::Number(first_number + 1))
        .await?
        .ok_or_else(|| eyre::eyre!("second block is missing"))?;
    match block_interval {
        DevnetBlockInterval::TwoSeconds => {
            eyre::ensure!(first.header.timestamp_ms.is_none());
            eyre::ensure!(second.header.timestamp == first.header.timestamp + 2);
        }
        DevnetBlockInterval::TwoHundredMilliseconds => {
            let first_ms = first.header.timestamp_ms.ok_or_else(|| eyre::eyre!("no millis"))?;
            let second_ms = second.header.timestamp_ms.ok_or_else(|| eyre::eyre!("no millis"))?;
            eyre::ensure!(
                second_ms == first_ms + 200,
                "blocks {first_ms} and {second_ms} are not 200ms apart"
            );
        }
    }
    Ok(())
}

/// Sends a signed EIP-1559 transfer from a genesis-funded Anvil account and waits for it.
async fn send_transfer_and_wait(provider: &RootProvider<Base>, chain_id: u64) -> Result<()> {
    let signer: PrivateKeySigner =
        format!("0x{}", hex::encode(ANVIL_ACCOUNT_1.private_key.as_slice())).parse()?;
    let sender = signer.address();
    let recipient = Address::repeat_byte(0xde);
    let sender_before = provider.get_balance(sender).await?;
    eyre::ensure!(sender_before > U256::ZERO, "genesis did not fund the test account");

    let transaction = BaseTransactionRequest::default()
        .from(sender)
        .to(recipient)
        .value(U256::from(TRANSFER_WEI))
        .transaction_type(2)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(10_000_000_000)
        .with_max_priority_fee_per_gas(1_000_000)
        .with_chain_id(chain_id)
        .with_nonce(provider.get_transaction_count(sender).await?)
        .build_typed_tx()
        .map_err(|_| eyre::eyre!("invalid light devnet test transaction"))?;
    let signature = signer.sign_hash_sync(&transaction.signature_hash())?;
    let signed = transaction.into_signed(signature);
    let hash = *signed.hash();
    let raw: Bytes = signed.encoded_2718().into();
    let pending =
        provider.send_raw_transaction(&raw).await.wrap_err("failed to submit the transfer")?;
    eyre::ensure!(*pending.tx_hash() == hash, "submitted transaction hash changed");

    let receipt = timeout(TX_TIMEOUT, async {
        loop {
            if let Some(receipt) = provider.get_transaction_receipt(hash).await? {
                return Ok::<_, eyre::Report>(receipt);
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .wrap_err("timed out waiting for the transfer to be included")??;
    eyre::ensure!(receipt.inner.inner.status(), "transfer reverted");
    assert_eq!(provider.get_balance(recipient).await?, U256::from(TRANSFER_WEI));
    Ok(())
}

/// Two-second blocks: the default cadence, generated genesis, no prefund.
#[tokio::test(flavor = "multi_thread")]
async fn light_devnet_advances_and_includes_transfer() -> Result<()> {
    run_light_devnet(DevnetBlockInterval::TwoSeconds, None).await
}

/// 200ms blocks activate Denim at the first block, including its `BaseTime` deposit, and honour
/// the one-time prefund.
#[tokio::test(flavor = "multi_thread")]
async fn light_devnet_subsecond_blocks_with_prefund() -> Result<()> {
    run_light_devnet(
        DevnetBlockInterval::TwoHundredMilliseconds,
        Some(DevnetPrefund { address: Address::repeat_byte(0x42), amount: 7_000_000_000_000 }),
    )
    .await
}
