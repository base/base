//! End-to-end test for gas-limit fee billing, the Zenith-gated proof of concept that charges a
//! transaction for its full declared `gas_limit` instead of the gas it consumed.
//!
//! The same over-provisioned transfer is sent before and after the Zenith activation on a real
//! builder + validator stack. Before Zenith the receipt and the sender's balance reflect the
//! 21k gas a transfer uses; after it they reflect the 100k gas limit, every fee recipient is paid
//! on the limit, `eth_estimateGas` still reports the real requirement, and the validator agrees
//! with the builder on every block.

mod common;

use std::time::Duration;

use alloy_consensus::{SignableTransaction, Transaction, constants::ETH_TO_WEI};
use alloy_eips::{BlockId, BlockNumberOrTag, eip2718::Encodable2718};
use alloy_network::{ReceiptResponse, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::Predeploys;
use base_common_genesis::{BaseUpgrade, RollupConfig};
use base_common_network::Base;
use base_common_rpc_types::{BaseTransactionReceipt, BaseTransactionRequest};
use base_system_tests::{ANVIL_ACCOUNT_1, SystemTestStackBuilder};
use eyre::{OptionExt, Result, WrapErr, ensure};
use tokio::time::{sleep, timeout};

use crate::common::{
    BASE_AZUL_ACTIVATION_BLOCK, BASE_BERYL_ACTIVATION_BLOCK, L1_CHAIN_ID, L2_CHAIN_ID,
    wait_for_block,
};

/// The schedule leading up to Zenith mirrors the Everest system tests (Everest at block 7), so
/// the post-Zenith chain carries every real upgrade; Zenith follows a few blocks later, leaving
/// pre-Zenith runway for the first transfer.
const BASE_COBALT_ACTIVATION_BLOCK: u64 = 5;
const BASE_EVEREST_ACTIVATION_BLOCK: u64 = 7;
const ZENITH_ACTIVATION_BLOCK: u64 = 14;
/// A plain transfer needs 21k gas; the limit is deliberately ~5x that.
const TRANSFER_GAS_USED: u64 = 21_000;
const TX_GAS_LIMIT: u64 = 100_000;
const TX_VALUE: U256 = U256::from_limbs([1, 0, 0, 0]);
const MAX_FEE_PER_GAS: u128 = 2_000_000_000;
const MAX_PRIORITY_FEE_PER_GAS: u128 = 1_000_000;
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

/// What one transfer cost, read from its receipt and from balances at its block and parent.
struct Settlement {
    tx_hash: B256,
    block_number: u64,
    block_timestamp: u64,
    gas_used: u64,
    effective_gas_price: u128,
    l1_fee: u128,
    /// How much the sender's balance dropped, including the transferred value.
    sender_paid: U256,
    coinbase_received: U256,
    base_fee_vault_received: U256,
    operator_fee_vault_received: U256,
}

impl Settlement {
    /// Everything the sender paid beyond the value and the L1 data fee, which must have landed
    /// at the three execution-fee recipients: billing on the limit neither mints nor burns ETH.
    fn execution_fee_paid(&self) -> U256 {
        self.sender_paid - TX_VALUE - U256::from(self.l1_fee)
    }

    fn execution_fee_received(&self) -> U256 {
        self.coinbase_received + self.base_fee_vault_received + self.operator_fee_vault_received
    }
}

#[tokio::test]
async fn zenith_bills_transactions_for_their_gas_limit() -> Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();

    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_base_azul_activation_block(BASE_AZUL_ACTIVATION_BLOCK)
        .with_base_beryl_activation_block(BASE_BERYL_ACTIVATION_BLOCK)
        .with_base_cobalt_activation_block(BASE_COBALT_ACTIVATION_BLOCK)
        .with_base_everest_activation_block(BASE_EVEREST_ACTIVATION_BLOCK)
        .with_base_zenith_activation_block(ZENITH_ACTIVATION_BLOCK)
        .build()
        .await?;
    let builder = system.l2_builder_provider()?;
    let client = system.l2_client_provider()?;
    wait_for_block(&builder, BASE_EVEREST_ACTIVATION_BLOCK + 1).await?;
    let signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)?;
    let rollup_config: RollupConfig =
        serde_json::from_slice(&std::fs::read(system.l2_deployment().rollup_config_path())?)?;
    let zenith_timestamp = rollup_config
        .upgrade_activation_timestamp(BaseUpgrade::Zenith)
        .ok_or_eyre("Zenith is not scheduled in the rollup config")?;

    // Before Zenith: the transfer is charged for the 21k gas it used and the rest of its
    // limit is reimbursed.
    let before = send_transfer(&builder, &signer).await.wrap_err("pre-Zenith transfer")?;
    ensure!(
        before.block_timestamp < zenith_timestamp,
        "pre-Zenith transfer landed at block {} after the Zenith activation",
        before.block_number
    );
    assert_eq!(before.gas_used, TRANSFER_GAS_USED, "pre-Zenith receipt must report gas used");
    assert_settlement_consistent(&before, TRANSFER_GAS_USED);

    // After Zenith: the identical transfer is charged for its full 100k gas limit. The receipt,
    // the block and every fee recipient agree on that quantity.
    wait_for_timestamp(&builder, zenith_timestamp).await?;
    let after = send_transfer(&builder, &signer).await.wrap_err("post-Zenith transfer")?;
    ensure!(
        after.block_timestamp >= zenith_timestamp,
        "post-Zenith transfer landed at block {} before the Zenith activation",
        after.block_number
    );
    assert_eq!(after.gas_used, TX_GAS_LIMIT, "post-Zenith receipt must report the gas limit");
    assert_settlement_consistent(&after, TX_GAS_LIMIT);
    assert_eq!(after.l1_fee, before.l1_fee, "the L1 data fee is independent of billing");
    assert!(
        after.sender_paid > before.sender_paid,
        "the over-provisioned transfer must cost more under gas-limit billing"
    );
    assert_block_gas_matches_receipts(&builder, after.block_number).await?;

    // Gas estimation is a simulation that bills nothing, so it still converges on the real
    // requirement rather than on the request's gas limit.
    let estimate = builder
        .estimate_gas(transfer_request(&signer, builder.get_transaction_count(signer.address()).await?))
        .await?;
    assert_eq!(estimate, TRANSFER_GAS_USED, "eth_estimateGas must keep reporting the real need");

    // The validator re-executes and must accept the builder's post-Zenith blocks: the fee
    // settlement and block gas accounting are consensus rules, so both nodes must agree.
    let last_block = after.block_number + 2;
    wait_for_block(&builder, last_block).await?;
    wait_for_block(&client, last_block).await?;
    for number in [before.block_number, after.block_number, last_block] {
        let builder_block = builder
            .get_block_by_number(BlockNumberOrTag::Number(number))
            .await?
            .ok_or_eyre("builder block missing")?;
        let client_block = client
            .get_block_by_number(BlockNumberOrTag::Number(number))
            .await?
            .ok_or_eyre("client block missing")?;
        assert_eq!(
            builder_block.header.hash, client_block.header.hash,
            "builder and validator disagree on block {number}"
        );
    }
    let client_receipt = client
        .get_transaction_receipt(after.tx_hash)
        .await?
        .ok_or_eyre("validator has no receipt for the post-Zenith transfer")?;
    assert_eq!(client_receipt.gas_used(), TX_GAS_LIMIT);

    Ok(())
}

/// The receipt-level fee identities that hold under both billing rules for `billed_gas`.
fn assert_settlement_consistent(settlement: &Settlement, billed_gas: u64) {
    let gas = U256::from(billed_gas);
    let base_fee = U256::from(settlement.effective_gas_price - MAX_PRIORITY_FEE_PER_GAS);
    assert_eq!(
        settlement.coinbase_received,
        gas * U256::from(MAX_PRIORITY_FEE_PER_GAS),
        "coinbase tip must be the priority fee on the billed gas"
    );
    assert_eq!(
        settlement.base_fee_vault_received,
        gas * base_fee,
        "base fee burn must be the base fee on the billed gas"
    );
    assert_eq!(
        settlement.execution_fee_paid(),
        settlement.execution_fee_received(),
        "every wei the sender paid in execution fees must reach a fee recipient"
    );
    assert_eq!(
        settlement.sender_paid,
        TX_VALUE
            + gas * U256::from(settlement.effective_gas_price)
            + U256::from(settlement.l1_fee)
            + settlement.operator_fee_vault_received,
        "sender must pay value + billed_gas * effective_gas_price + L1 fee + operator fee"
    );
}

/// The block header's `gas_used` is the receipts' last `cumulative_gas_used`, and that cumulative
/// sum is made of per-receipt `gas_used` values — so a billed-on-limit receipt shows up in the
/// header, which is what lets a block's gas fullness be known from gas limits alone.
async fn assert_block_gas_matches_receipts(
    provider: &RootProvider<Base>,
    block_number: u64,
) -> Result<()> {
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(block_number))
        .await?
        .ok_or_eyre("block missing")?;
    let receipts = provider
        .get_block_receipts(BlockId::number(block_number))
        .await?
        .ok_or_eyre("block receipts missing")?;
    let summed: u64 = receipts.iter().map(|receipt| receipt.gas_used()).sum();
    assert_eq!(block.header.gas_used, summed, "header gas_used must be the sum of receipts");
    assert!(
        receipts.iter().any(|receipt| receipt.gas_used() == TX_GAS_LIMIT),
        "the post-Zenith block must carry a receipt billed at the gas limit"
    );
    assert!(block.header.gas_used >= TX_GAS_LIMIT);
    Ok(())
}

fn transfer_request(signer: &PrivateKeySigner, nonce: u64) -> BaseTransactionRequest {
    BaseTransactionRequest::default()
        .from(signer.address())
        .to(Address::repeat_byte(0xfe))
        .value(TX_VALUE)
        .transaction_type(2)
        .with_gas_limit(TX_GAS_LIMIT)
        .with_max_fee_per_gas(MAX_FEE_PER_GAS)
        .with_max_priority_fee_per_gas(MAX_PRIORITY_FEE_PER_GAS)
        .with_chain_id(L2_CHAIN_ID)
        .with_nonce(nonce)
}

/// Sends the over-provisioned transfer and reads back how it was settled.
async fn send_transfer(provider: &RootProvider<Base>, signer: &PrivateKeySigner) -> Result<Settlement> {
    let sender = signer.address();
    let nonce = provider.get_transaction_count(sender).await?;
    let transaction = transfer_request(signer, nonce)
        .build_typed_tx()
        .map_err(|error| eyre::eyre!("invalid transaction: {error:?}"))?;
    ensure!(transaction.gas_limit() == TX_GAS_LIMIT);
    let signature = signer.sign_hash_sync(&transaction.signature_hash())?;
    let raw_transaction: Bytes = transaction.into_signed(signature).encoded_2718().into();
    let pending = provider.send_raw_transaction(&raw_transaction).await?;
    let tx_hash = *pending.tx_hash();
    drop(pending);

    let receipt = wait_for_receipt(provider, tx_hash).await?;
    let block_number = receipt.block_number().ok_or_eyre("receipt missing block number")?;
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(block_number))
        .await?
        .ok_or_eyre("receipt block missing")?;
    ensure!(
        block.transactions.len() == 2,
        "expected only the L1 attributes deposit and the transfer in block {block_number}"
    );

    let delta = async |address: Address| -> Result<U256> {
        let before = provider.get_balance(address).block_id(BlockId::number(block_number - 1)).await?;
        let after = provider.get_balance(address).block_id(BlockId::number(block_number)).await?;
        Ok(after.abs_diff(before))
    };
    let sender_paid = delta(sender).await?;
    ensure!(sender_paid < U256::from(ETH_TO_WEI), "sender paid an implausible amount");

    Ok(Settlement {
        tx_hash,
        block_number,
        block_timestamp: block.header.timestamp,
        gas_used: receipt.gas_used(),
        effective_gas_price: receipt.effective_gas_price(),
        l1_fee: receipt.l1_block_info.l1_fee.ok_or_eyre("receipt missing l1Fee")?,
        sender_paid,
        coinbase_received: delta(block.header.beneficiary).await?,
        base_fee_vault_received: delta(Predeploys::BASE_FEE_VAULT).await?,
        operator_fee_vault_received: delta(Predeploys::OPERATOR_FEE_VAULT).await?,
    })
}

async fn wait_for_receipt(
    provider: &RootProvider<Base>,
    tx_hash: B256,
) -> Result<BaseTransactionReceipt> {
    timeout(RECEIPT_TIMEOUT, async {
        loop {
            if let Some(receipt) = provider.get_transaction_receipt(tx_hash).await? {
                return Ok::<_, eyre::Error>(receipt);
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .wrap_err("transaction receipt timed out")?
}

async fn wait_for_timestamp(provider: &RootProvider<Base>, timestamp: u64) -> Result<u64> {
    timeout(Duration::from_secs(90), async {
        loop {
            let block = provider
                .get_block_by_number(BlockNumberOrTag::Latest)
                .await?
                .ok_or_eyre("latest block missing")?;
            if block.header.timestamp >= timestamp {
                return Ok::<_, eyre::Error>(block.header.number);
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .wrap_err_with(|| format!("timed out waiting for timestamp {timestamp}"))?
}
