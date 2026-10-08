//! Local integration coverage for the snapshot-backed development network.

use std::{sync::Arc, time::Duration};

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_genesis::GenesisAccount;
use alloy_network::{ReceiptResponse, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, U64, U256, bytes, keccak256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{BlockId, BlockNumberOrTag};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, sol};
use base_common_consensus::{BaseTxEnvelope, Predeploys};
use base_common_genesis::{BaseUpgrade, RollupConfig, SystemConfig};
use base_common_network::Base;
use base_common_rpc_types::{BaseTransactionReceipt, BaseTransactionRequest};
use base_consensus_derive::AttributesBuilder;
use base_consensus_node::{StandaloneAttributesBuilder, StandalonePrefund};
use base_execution_chainspec::BaseChainSpec;
use base_node_runner::test_utils::{
    BLOCK_TIME_SECONDS, GAS_LIMIT, L1_BLOCK_INFO_DEPOSIT_TX, TestHarnessBuilder,
};
use base_protocol::{BlockInfo, L1BlockInfoTx, L2BlockInfo};
use base_system_tests::{
    ANVIL_ACCOUNT_1, DevnetBlockInterval, DevnetConfig, DevnetL2State, DevnetPrefund,
    SnapshotChainConfig, SnapshotImpersonation, SnapshotImpersonationAttributesBuilder,
    SnapshotL2Stack, SystemTestStackBuilder,
};
use base_test_utils::{Account, build_test_genesis};
use eyre::{OptionExt, Result, WrapErr};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};

const TX_TIMEOUT: Duration = Duration::from_secs(30);
const IMPERSONATE_METHOD: &str = "dev_impersonateTransaction";
const METHOD_NOT_FOUND_CODE: i64 = -32601;
const TRANSFER_GAS: u64 = 21_000;
const CALL_GAS: u64 = 200_000;

/// Keyless account funded only by the standalone prefund deposit.
const WHALE: Address = Address::repeat_byte(0x11);
const WHALE_FUNDING_WEI: u128 = 1_000_000_000_000_000_000;
/// Keyless, unfunded account whose value transfer must fail without minting.
const UNFUNDED: Address = Address::repeat_byte(0x33);
const RECIPIENT: Address = Address::repeat_byte(0x22);
const TRANSFER_WEI: u64 = 1_000;
const CALL_WEI: u64 = 7;
const CALL_WORD: B256 = B256::repeat_byte(0x2a);
/// Records `CALLER`, `CALLVALUE`, and the first calldata word in storage slots 0, 1, and 2.
const RECORDER: Address = Address::repeat_byte(0xc0);
const RECORDER_CODE: Bytes = bytes!("336000553460015560003560025500");

sol! {
    function deposit() payable;
    function balanceOf(address owner) view returns (uint256);
}

/// Impersonated requests become canonical deposits exactly once on a real execution node.
#[tokio::test]
async fn impersonated_requests_are_included_once_by_real_execution_node() -> Result<()> {
    let mut genesis = build_test_genesis();
    genesis.alloc.insert(RECORDER, GenesisAccount::default().with_code(Some(RECORDER_CODE)));
    let genesis_time = genesis.timestamp;
    let queue = SnapshotImpersonation::new(GAS_LIMIT);
    let harness = TestHarnessBuilder::new()
        .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(genesis)))
        .with_extension(queue.clone())
        .build()
        .await?;
    let provider = harness.provider();
    let rpc = harness.rpc_client()?;

    let l1_info = L1BlockInfoTx::decode_calldata(
        BaseTxEnvelope::decode_2718(&mut L1_BLOCK_INFO_DEPOSIT_TX.as_ref())?
            .as_deposit()
            .ok_or_eyre("L1-info fixture must be a deposit")?
            .input
            .as_ref(),
    )?;
    let mut rollup = RollupConfig { block_time: BLOCK_TIME_SECONDS, ..Default::default() };
    rollup.genesis.l2_time = genesis_time;
    // ponytail: only Regolith changes the encoded deposits; the harness supplies header fields.
    rollup.set_upgrade_activation_timestamp(BaseUpgrade::Regolith, 0);
    let mut inner = StandaloneAttributesBuilder::new(
        Arc::new(rollup),
        l1_info,
        SystemConfig { gas_limit: GAS_LIMIT, ..Default::default() },
        Some(StandalonePrefund { address: WHALE, amount: WHALE_FUNDING_WEI }),
    );
    let mut builder =
        SnapshotImpersonationAttributesBuilder::new(inner.clone(), queue, provider.clone());

    let missing_gas: Result<B256, _> =
        rpc.request(IMPERSONATE_METHOD, json!([{ "from": WHALE, "to": RECIPIENT }])).await;
    assert!(missing_gas.is_err(), "gas is a required request field");

    let transfer = || transfer_request(WHALE, RECIPIENT, TRANSFER_WEI);
    let failing = impersonate(&rpc, transfer_request(UNFUNDED, RECIPIENT, TRANSFER_WEI)).await?;
    let first_transfer = impersonate(&rpc, transfer()).await?;
    let call = impersonate(
        &rpc,
        json!({
            "from": WHALE,
            "to": RECORDER,
            "gas": U64::from(CALL_GAS),
            "value": U256::from(CALL_WEI),
            "data": CALL_WORD,
        }),
    )
    .await?;

    let (transactions, appended) =
        next_transactions(&mut builder, &mut inner, &provider, l1_info).await?;
    assert_eq!(appended, [failing, first_transfer, call]);
    // A valid sibling that never becomes canonical must not retire its requests.
    harness.prepare_unsafe_block(transactions).await?;

    let identical_transfer = impersonate(&rpc, transfer()).await?;
    assert_ne!(identical_transfer, first_transfer, "identical requests must stay distinct");
    let (mut transactions, appended) =
        next_transactions(&mut builder, &mut inner, &provider, l1_info).await?;
    assert_eq!(appended, [failing, first_transfer, call, identical_transfer]);
    let (signed, signed_hash) = Account::Alice
        .sign_txn_request(BaseTransactionRequest::default().to(Account::Bob.address()).nonce(0))?;
    transactions.push(signed);
    harness.build_block_from_transactions(transactions).await?;

    for (hash, from, success) in [
        (failing, UNFUNDED, false),
        (first_transfer, WHALE, true),
        (call, WHALE, true),
        (identical_transfer, WHALE, true),
        (signed_hash, Account::Alice.address(), true),
    ] {
        assert_receipt(&provider, hash, 1, from, success).await?;
    }
    assert_eq!(provider.get_balance(UNFUNDED).await?, U256::ZERO);
    assert_eq!(provider.get_balance(RECIPIENT).await?, U256::from(2 * TRANSFER_WEI));
    assert_eq!(provider.get_balance(RECORDER).await?, U256::from(CALL_WEI));
    assert_eq!(
        provider.get_balance(WHALE).await?,
        U256::from(WHALE_FUNDING_WEI - u128::from(2 * TRANSFER_WEI + CALL_WEI))
    );
    assert_eq!(
        provider.get_storage_at(RECORDER, U256::ZERO).await?,
        U256::from_be_bytes(WHALE.into_word().0)
    );
    assert_eq!(provider.get_storage_at(RECORDER, U256::from(1)).await?, U256::from(CALL_WEI));
    assert_eq!(
        provider.get_storage_at(RECORDER, U256::from(2)).await?,
        U256::from_be_bytes(CALL_WORD.0)
    );

    let later_transfer = impersonate(&rpc, transfer()).await?;
    assert!(![first_transfer, identical_transfer].contains(&later_transfer));
    let (transactions, appended) =
        next_transactions(&mut builder, &mut inner, &provider, l1_info).await?;
    assert_eq!(appended, [later_transfer], "included requests must not be replayed");
    harness.build_block_from_transactions(transactions).await?;
    assert_receipt(&provider, later_transfer, 2, WHALE, true).await?;
    assert_eq!(provider.get_balance(RECIPIENT).await?, U256::from(3 * TRANSFER_WEI));

    let (_, appended) = next_transactions(&mut builder, &mut inner, &provider, l1_info).await?;
    assert!(appended.is_empty(), "the queue must drain after canonical inclusion");
    Ok(())
}

fn transfer_request(from: Address, to: Address, value: u64) -> Value {
    json!({ "from": from, "to": to, "gas": U64::from(TRANSFER_GAS), "value": U256::from(value) })
}

async fn impersonate(rpc: &RpcClient, request: Value) -> Result<B256> {
    Ok(rpc.request(IMPERSONATE_METHOD, json!([request])).await?)
}

/// Prepares attributes on the canonical head and returns them with the appended request hashes.
///
/// The wrapper must keep the inner standalone transactions as an unchanged prefix, keep using the
/// transaction pool, and append only mint-free deposits.
async fn next_transactions(
    builder: &mut SnapshotImpersonationAttributesBuilder,
    inner: &mut StandaloneAttributesBuilder,
    provider: &RootProvider<Base>,
    l1_info: L1BlockInfoTx,
) -> Result<(Vec<Bytes>, Vec<B256>)> {
    let head = provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .ok_or_eyre("canonical head is missing")?
        .header;
    let parent = L2BlockInfo::new(
        BlockInfo::new(head.hash, head.number, head.parent_hash, head.timestamp),
        l1_info.id(),
        l1_info.sequence_number() + head.number,
    );
    let expected_prefix = inner
        .prepare_payload_attributes(parent, l1_info.id())
        .await?
        .transactions
        .unwrap_or_default();
    let attributes = builder.prepare_payload_attributes(parent, l1_info.id()).await?;
    assert_eq!(attributes.no_tx_pool, Some(false), "signed pool transactions must stay enabled");
    let transactions = attributes.transactions.unwrap_or_default();
    assert_eq!(transactions[..expected_prefix.len()], expected_prefix[..]);

    let mut appended = Vec::new();
    for transaction in &transactions[expected_prefix.len()..] {
        let envelope = BaseTxEnvelope::decode_2718(&mut transaction.as_ref())?;
        let deposit = envelope.as_deposit().ok_or_eyre("impersonated request must be a deposit")?;
        assert_eq!(deposit.mint, 0, "impersonated requests must not mint");
        appended.push(keccak256(transaction));
    }
    Ok((transactions, appended))
}

async fn assert_receipt(
    provider: &RootProvider<Base>,
    hash: B256,
    block_number: u64,
    from: Address,
    success: bool,
) -> Result<()> {
    let receipt =
        provider.get_transaction_receipt(hash).await?.ok_or_eyre("canonical receipt missing")?;
    assert_eq!(receipt.inner.transaction_hash, hash);
    assert_eq!(receipt.inner.block_number, Some(block_number));
    assert_eq!(receipt.inner.from, from);
    assert_eq!(receipt.status(), success, "unexpected status for {hash}");
    Ok(())
}

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
    snapshot.enable_impersonation = true;
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
    let sender = signer.address();
    send_transaction_and_wait_for_both(&stack, signer).await?;
    impersonate_and_wait_for_both(&stack, sender).await?;
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

    wait_for_receipt_on_both(stack, hash).await?;
    Ok(())
}

/// Submits an unsigned transfer and WETH deposit to the builder and checks follower convergence.
async fn impersonate_and_wait_for_both(stack: &SnapshotL2Stack, sender: Address) -> Result<()> {
    let builder = RootProvider::<Base>::new_http(stack.builder_rpc_url()?);
    let client = RootProvider::<Base>::new_http(stack.client_rpc_url()?);
    let builder_rpc = RpcClient::new_http(stack.builder_rpc_url()?);
    let client_rpc = RpcClient::new_http(stack.client_rpc_url()?);
    let transfer = transfer_request(sender, Address::repeat_byte(0xde), 1);

    let follower: Result<B256, _> =
        client_rpc.request(IMPERSONATE_METHOD, json!([transfer.clone()])).await;
    assert_eq!(
        follower.unwrap_err().as_error_resp().map(|error| error.code),
        Some(METHOD_NOT_FOUND_CODE),
        "followers must not expose impersonation"
    );

    let weth_before = weth_balance(&builder, sender, BlockId::latest()).await?;
    let transfer = impersonate(&builder_rpc, transfer).await?;
    let call = impersonate(
        &builder_rpc,
        json!({
            "from": sender,
            "to": Predeploys::WETH9,
            "gas": U64::from(CALL_GAS),
            "value": U256::from(CALL_WEI),
            "data": Bytes::from(depositCall {}.abi_encode()),
        }),
    )
    .await?;

    let receipts = [
        wait_for_receipt_on_both(stack, transfer).await?,
        wait_for_receipt_on_both(stack, call).await?,
    ];
    for receipt in &receipts {
        eyre::ensure!(receipt.inner.from == sender, "impersonated sender changed");
        eyre::ensure!(receipt.status(), "impersonated request failed");
    }
    let call_block = receipts[1].inner.block_hash.ok_or_eyre("WETH deposit block is missing")?;
    for provider in [&builder, &client] {
        let after = weth_balance(provider, sender, BlockId::hash(call_block)).await?;
        eyre::ensure!(after == weth_before + U256::from(CALL_WEI), "WETH deposit missing");
    }
    Ok(())
}

async fn weth_balance(
    provider: &RootProvider<Base>,
    owner: Address,
    block: BlockId,
) -> Result<U256> {
    let request = BaseTransactionRequest::default()
        .to(Predeploys::WETH9)
        .input(Bytes::from(balanceOfCall { owner }.abi_encode()).into());
    Ok(U256::from_be_slice(&provider.call(request).block(block).await?))
}

async fn wait_for_receipt_on_both(
    stack: &SnapshotL2Stack,
    hash: B256,
) -> Result<BaseTransactionReceipt> {
    let builder = RootProvider::<Base>::new_http(stack.builder_rpc_url()?);
    let client = RootProvider::<Base>::new_http(stack.client_rpc_url()?);
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
    Ok(builder_receipt)
}
