//! Native Denim integration test for builder-recorded validity transaction status.
//!
//! Launches real nodes with the multiplexing payload service in basic-only and Denim cutover
//! modes, submits transactions over HTTP JSON-RPC, drives blocks through the Engine API IPC
//! endpoint, and queries `base_getValidityTransactionStatus` against the canonical chain.

use std::{
    net::TcpListener,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::SignableTransaction;
use alloy_eips::{eip2718::Encodable2718, eip7685::Requests};
use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, B64, B256, Bytes, TxHash, TxKind, U256, address};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_builder_core::BuilderConfig;
use base_builder_multiplex::MultiplexingServiceBuilder;
use base_common_consensus::{BaseTimeUpdateTx, BaseTxEnvelope};
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_execution_chainspec::BaseChainSpec;
use base_execution_payload_builder::BasePayloadBuilderAttributes;
use base_execution_rpc::BaseEngineApiClient;
use base_execution_txpool::{ValidityOperator, ValidityPredicate};
use base_execution_validity_metadata::{
    DEFAULT_VALIDITY_METADATA_QUEUE_CAPACITY, ValidityMetadataRecorder, ValidityMetadataStore,
};
use base_node_core::{BaseEngineTypes, args::RollupArgs};
use base_node_runner::{BaseNodeRunner, LaunchedBaseNode, test_utils::L1_BLOCK_INFO_DEPOSIT_TX};
use base_test_utils::{Account, DEVNET_CHAIN_ID, build_test_genesis};
use base_txpool_rpc::{
    SendRawTransactionValidityConfig, SendRawTransactionValidityExtension,
    SendRawTransactionValidityOptions, ValidityTransactionStatusConfig,
    ValidityTransactionStatusExtension,
};
use reth_node_builder::NodeBuilder;
use reth_tasks::{Runtime, RuntimeBuilder, RuntimeConfig};
use serde_json::{Value, json};

/// Contract whose code is `PUSH1 0 PUSH1 0 REVERT`.
const REVERTER: Address = address!("0x00000000000000000000000000000000000000fd");
/// Account whose balance never matches [`UNSATISFIABLE_BALANCE`].
const WATCHED: Address = address!("0x000000000000000000000000000000000000dEaD");
const UNSATISFIABLE_BALANCE: u64 = 12_345;
/// Anvil account #4, funded only in this test's genesis.
const DEFERRED_KEY: &str = "47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a";
/// Anvil account #5, funded only in this test's genesis.
const EXPIRING_KEY: &str = "8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba";
const GAS_LIMIT: u64 = 30_000_000;
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const UNAVAILABLE_CODE: i64 = -32050;

fn signer(key: &str) -> PrivateKeySigner {
    key.parse().expect("valid test key")
}

/// Denim-from-genesis chain with funded senders and a reverting contract.
fn denim_chain_spec() -> Arc<BaseChainSpec> {
    let mut genesis = build_test_genesis();
    genesis.config.extra_fields.insert("base".into(), json!({ "denim": 0 }));
    let balance = U256::from(10).pow(U256::from(24));
    for key in [DEFERRED_KEY, EXPIRING_KEY] {
        genesis
            .alloc
            .insert(signer(key).address(), GenesisAccount::default().with_balance(balance));
    }
    genesis.alloc.insert(
        REVERTER,
        GenesisAccount::default()
            .with_code(Some(Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xfd]))),
    );
    Arc::new(BaseChainSpec::from_genesis(genesis))
}

/// Signs an EIP-1559 transaction with a nonzero tip.
fn sign(signer: &PrivateKeySigner, nonce: u64, to: Address) -> (Bytes, TxHash) {
    let tx = alloy_consensus::TxEip1559 {
        chain_id: DEVNET_CHAIN_ID,
        nonce,
        gas_limit: 100_000,
        max_fee_per_gas: 10_000_000_000,
        max_priority_fee_per_gas: 1_000,
        to: TxKind::Call(to),
        value: U256::from(1),
        ..Default::default()
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).expect("sign");
    let signed = BaseTxEnvelope::Eip1559(tx.into_signed(signature));
    (signed.encoded_2718().into(), signed.tx_hash())
}

fn block_number_at_most(value: u64) -> ValidityPredicate {
    ValidityPredicate::BlockNumber {
        op: ValidityOperator::LessThanOrEqual,
        value: U256::from(value),
    }
}

fn unsatisfiable() -> ValidityPredicate {
    ValidityPredicate::Balance {
        address: WATCHED,
        op: ValidityOperator::Equal,
        value: U256::from(UNSATISFIABLE_BALANCE),
    }
}

/// Signed transactions with their expected classification roles.
struct Fixture {
    matched: (Bytes, TxHash),
    reverted: (Bytes, TxHash),
    plain: (Bytes, TxHash),
    deferred: (Bytes, TxHash),
    expiring: (Bytes, TxHash),
    /// Plain transaction included through `no_tx_pool` attributes.
    derived: (Bytes, TxHash),
    /// Validity transaction included in a block that is imported before it is canonical.
    late: (Bytes, TxHash),
}

impl Fixture {
    fn new() -> Self {
        Self {
            matched: sign(&Account::Alice.signer(), 0, Account::Bob.address()),
            reverted: sign(&Account::Bob.signer(), 0, REVERTER),
            plain: sign(&Account::Charlie.signer(), 0, Account::Alice.address()),
            deferred: sign(&signer(DEFERRED_KEY), 0, Account::Alice.address()),
            expiring: sign(&signer(EXPIRING_KEY), 0, Account::Alice.address()),
            derived: sign(&Account::Deployer.signer(), 0, Account::Alice.address()),
            late: sign(&Account::Alice.signer(), 1, Account::Bob.address()),
        }
    }

    /// Submits the block-1 candidates in a fixed order.
    async fn submit_initial(&self, node: &ValidityNode) -> eyre::Result<()> {
        node.send_validity(&self.matched.0, vec![block_number_at_most(20)]).await?;
        node.send_validity(&self.reverted.0, vec![block_number_at_most(20)]).await?;
        let hash: TxHash =
            node.rpc.request("eth_sendRawTransaction", (self.plain.0.clone(),)).await?;
        assert_eq!(hash, self.plain.1);
        node.send_validity(&self.deferred.0, vec![unsatisfiable(), block_number_at_most(20)])
            .await?;
        node.send_validity(&self.expiring.0, vec![unsatisfiable(), block_number_at_most(1)])
            .await?;
        Ok(())
    }
}

struct ValidityNode {
    rpc: RpcClient,
    auth_ipc_path: String,
    chain_spec: Arc<BaseChainSpec>,
    _runtime: Runtime,
    _handle: LaunchedBaseNode,
}

impl ValidityNode {
    async fn launch(
        service_builder: MultiplexingServiceBuilder,
        store: Option<ValidityMetadataStore>,
        root: &Path,
    ) -> eyre::Result<Self> {
        let runtime = RuntimeBuilder::new(RuntimeConfig::default()).build()?;
        let chain_spec = denim_chain_spec();
        let mut node_config =
            reth_node_builder::NodeConfig::new(Arc::clone(&chain_spec)).with_unused_ports();
        node_config.rpc = reth_node_core::args::RpcServerArgs::default()
            .with_unused_ports()
            .with_http()
            .with_auth_ipc();
        node_config.rpc.ws = false;
        node_config.network.discovery.disable_discovery = true;
        node_config.engine.persistence_threshold = 0;
        let data_path = root.join("datadir");
        std::fs::create_dir_all(&data_path)?;
        node_config.rpc.ipcpath = data_path.join("rpc.ipc").to_string_lossy().into_owned();
        node_config.rpc.auth_ipc_path = data_path.join("engine.ipc").to_string_lossy().into_owned();
        node_config = node_config.with_datadir_args(reth_node_core::args::DatadirArgs {
            datadir: data_path.to_string_lossy().parse()?,
            static_files_path: None,
            rocksdb_path: Some(root.join("rocksdb")),
            pprof_dumps_path: Some(root.join("pprof")),
        });
        let db_path = root.join("db");
        std::fs::create_dir_all(&db_path)?;
        let db = reth_db::init_db(
            db_path.as_path(),
            reth_db::mdbx::DatabaseArguments::new(reth_db::ClientVersion::default()),
        )?;

        let mut runner =
            BaseNodeRunner::new(RollupArgs::default()).with_service_builder(service_builder);
        runner.install_ext::<SendRawTransactionValidityExtension>(
            SendRawTransactionValidityConfig::default(),
        );
        runner.install_ext::<ValidityTransactionStatusExtension>(ValidityTransactionStatusConfig {
            store,
        });
        let builder = NodeBuilder::new(node_config.clone())
            .with_database(db)
            .with_launch_context(runtime.clone());
        let handle = runner.launch(builder).await?;
        let http = handle
            .handle
            .node
            .rpc_server_handle()
            .http_local_addr()
            .ok_or_else(|| eyre::eyre!("HTTP RPC server did not bind"))?;
        assert!(http.ip().is_loopback(), "test RPC must bind to localhost only");

        Ok(Self {
            rpc: RpcClient::new_http(format!("http://{http}").parse()?),
            auth_ipc_path: node_config.rpc.auth_ipc_path,
            chain_spec,
            _runtime: runtime,
            _handle: handle,
        })
    }

    async fn send_validity(
        &self,
        raw: &Bytes,
        validity: Vec<ValidityPredicate>,
    ) -> eyre::Result<TxHash> {
        Ok(self
            .rpc
            .request(
                "base_sendRawTransactionValidity",
                (
                    raw.clone(),
                    SendRawTransactionValidityOptions { validity, validity_signature: None },
                ),
            )
            .await?)
    }

    async fn latest(&self) -> eyre::Result<Value> {
        Ok(self.rpc.request("eth_getBlockByNumber", ("latest", false)).await?)
    }

    /// Builds a block on the canonical head and imports it, optionally making it canonical.
    async fn build_block(
        &self,
        no_tx_pool: bool,
        extra_attribute_transactions: Vec<Bytes>,
        canonicalize: bool,
    ) -> eyre::Result<B256> {
        let latest = self.latest().await?;
        let parent_hash: B256 = serde_json::from_value(latest["hash"].clone())?;
        let number = u64::from_str_radix(
            latest["number"].as_str().unwrap_or_default().trim_start_matches("0x"),
            16,
        )? + 1;
        let schedule = self.chain_spec.denim_timestamp_schedule()?.expect("Denim is scheduled");
        let (timestamp, millis) = schedule.block_timestamp_parts(number);
        let base_time = BaseTimeUpdateTx::new(millis)?.into_deposit_tx(number);
        let mut transactions = vec![L1_BLOCK_INFO_DEPOSIT_TX, base_time.encoded_2718().into()];
        transactions.extend(extra_attribute_transactions);
        let min_base_fee = u64::from_str_radix(
            latest["baseFeePerGas"].as_str().unwrap_or("0x0").trim_start_matches("0x"),
            16,
        )?;
        let params = self.chain_spec.base_fee_params_at_timestamp(timestamp);
        let eip_1559_params =
            ((params.max_change_denominator as u64) << 32) | (params.elasticity_multiplier as u64);
        let attributes = BasePayloadBuilderAttributes::<BaseTxEnvelope>::try_new(
            parent_hash,
            BasePayloadAttributes {
                payload_attributes: PayloadAttributes {
                    timestamp,
                    parent_beacon_block_root: Some(B256::ZERO),
                    withdrawals: Some(vec![]),
                    slot_number: None,
                    ..Default::default()
                },
                transactions: Some(transactions),
                gas_limit: Some(GAS_LIMIT),
                no_tx_pool: Some(no_tx_pool),
                min_base_fee: Some(min_base_fee),
                eip_1559_params: Some(B64::from(eip_1559_params)),
            },
            3,
        )?;

        let engine =
            reth_ipc::client::IpcClientBuilder::default().build(&self.auth_ipc_path).await?;
        let head = ForkchoiceState {
            head_block_hash: parent_hash,
            safe_block_hash: parent_hash,
            finalized_block_hash: parent_hash,
        };
        let fcu = BaseEngineApiClient::<BaseEngineTypes>::fork_choice_updated_v3(
            &engine,
            head,
            Some(attributes),
        )
        .await?;
        let payload_id = fcu.payload_id.ok_or_else(|| eyre::eyre!("missing payload id"))?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let envelope =
            BaseEngineApiClient::<BaseEngineTypes>::get_payload_v4(&engine, payload_id).await?;
        let status = BaseEngineApiClient::<BaseEngineTypes>::new_payload_v4(
            &engine,
            envelope.execution_payload,
            vec![],
            B256::ZERO,
            Requests::default(),
        )
        .await?;
        eyre::ensure!(status.is_valid(), "payload rejected: {status:?}");
        let block_hash = status.latest_valid_hash.ok_or_else(|| eyre::eyre!("missing hash"))?;
        if canonicalize {
            self.canonicalize(block_hash).await?;
        }
        Ok(block_hash)
    }

    async fn canonicalize(&self, block_hash: B256) -> eyre::Result<()> {
        let engine =
            reth_ipc::client::IpcClientBuilder::default().build(&self.auth_ipc_path).await?;
        BaseEngineApiClient::<BaseEngineTypes>::fork_choice_updated_v3(
            &engine,
            ForkchoiceState {
                head_block_hash: block_hash,
                safe_block_hash: block_hash,
                finalized_block_hash: block_hash,
            },
            None,
        )
        .await?;
        let deadline = Instant::now() + WAIT_TIMEOUT;
        while self.latest().await?["hash"] != json!(block_hash) {
            eyre::ensure!(Instant::now() < deadline, "block {block_hash} did not become canonical");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }

    async fn status(&self, tx_hash: TxHash) -> Result<Value, (i64, String, Option<String>)> {
        self.rpc.request::<_, Value>("base_getValidityTransactionStatus", (tx_hash,)).await.map_err(
            |error| {
                let payload = error.as_error_resp().expect("JSON-RPC error response");
                let data = payload.data.as_ref().map(|data| data.get().to_string());
                (payload.code, payload.message.to_string(), data)
            },
        )
    }

    async fn block_transactions(&self, block_hash: B256) -> eyre::Result<Vec<TxHash>> {
        let block: Value = self.rpc.request("eth_getBlockByHash", (block_hash, false)).await?;
        Ok(serde_json::from_value(block["transactions"].clone())?)
    }
}

fn spawn_recorder(store: &ValidityMetadataStore) -> ValidityMetadataRecorder {
    ValidityMetadataRecorder::spawn(store.clone(), DEFAULT_VALIDITY_METADATA_QUEUE_CAPACITY)
        .expect("spawn validity metadata writer")
        .0
}

fn builder_config() -> BuilderConfig {
    let mut config = BuilderConfig::default();
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    config.flashblocks_ws_addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    config
}

/// Waits for the writer thread to commit the record of `block_hash`.
async fn wait_for_record(store: &ValidityMetadataStore, block_hash: B256) -> eyre::Result<()> {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    while store.get(block_hash)?.is_none() {
        eyre::ensure!(Instant::now() < deadline, "record for {block_hash} was not written");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

fn assert_status(status: &Value, tx_hash: TxHash, block_hash: B256, index: usize, validity: bool) {
    assert_eq!(
        status,
        &json!({
            "transactionHash": tx_hash,
            "blockHash": block_hash,
            "blockNumber": "0x1",
            "transactionIndex": format!("{index:#x}"),
            "isValidityTransaction": validity,
        })
    );
}

fn assert_unavailable(result: Result<Value, (i64, String, Option<String>)>) {
    let (code, message, data) = result.expect_err("metadata must be unavailable");
    assert_eq!(code, UNAVAILABLE_CODE);
    assert_eq!(message, "validity metadata unavailable");
    assert_eq!(data.as_deref(), Some("\"not_recorded\""));
}

/// Runs the full classification flow on an enabled node and returns the first pool block hash.
async fn assert_recorded_classification(
    node: &ValidityNode,
    store: &ValidityMetadataStore,
) -> eyre::Result<B256> {
    let fixture = Fixture::new();
    fixture.submit_initial(node).await?;

    let block = node.build_block(false, vec![], true).await?;
    wait_for_record(store, block).await?;
    let transactions = node.block_transactions(block).await?;
    assert_eq!(transactions.len(), 5, "deposits, matched, reverted and plain");
    let mut pool_transactions = transactions[2..].to_vec();
    pool_transactions.sort();
    let mut expected = vec![fixture.matched.1, fixture.reverted.1, fixture.plain.1];
    expected.sort();
    assert_eq!(pool_transactions, expected);
    let index = |hash: TxHash| transactions.iter().position(|tx| *tx == hash).unwrap();

    let receipt: Value =
        node.rpc.request("eth_getTransactionReceipt", (fixture.reverted.1,)).await?;
    assert_eq!(receipt["status"], "0x0", "the reverted validity transaction must revert");

    for (hash, validity) in
        [(fixture.matched.1, true), (fixture.reverted.1, true), (fixture.plain.1, false)]
    {
        assert_status(&node.status(hash).await.unwrap(), hash, block, index(hash), validity);
    }
    assert_status(&node.status(transactions[0]).await.unwrap(), transactions[0], block, 0, false);
    assert_status(&node.status(transactions[1]).await.unwrap(), transactions[1], block, 1, false);
    assert_eq!(node.status(fixture.deferred.1).await.unwrap(), Value::Null);
    assert_eq!(node.status(TxHash::repeat_byte(0x42)).await.unwrap(), Value::Null);

    let raw: Bytes = node.rpc.request("eth_getRawTransactionByHash", (fixture.matched.1,)).await?;
    assert_eq!(raw, fixture.matched.0, "signed bytes must be unchanged");

    // Block 2 re-executes derived attributes: never recorded, so its transaction is unavailable.
    let derived = node.build_block(true, vec![fixture.derived.0.clone()], true).await?;
    assert!(node.block_transactions(derived).await?.contains(&fixture.derived.1));
    assert_unavailable(node.status(fixture.derived.1).await);
    assert!(store.get(derived)?.is_none());

    // The expiring transaction expired after block 1 and the deferred one stays pending.
    assert_eq!(node.status(fixture.expiring.1).await.unwrap(), Value::Null);
    assert_eq!(node.status(fixture.deferred.1).await.unwrap(), Value::Null);

    // A recorded block that is imported but not canonical never reveals its transactions.
    node.send_validity(&fixture.late.0, vec![block_number_at_most(20)]).await?;
    let late_block = node.build_block(false, vec![], false).await?;
    wait_for_record(store, late_block).await?;
    assert!(store.get(late_block)?.expect("record").contains(&fixture.late.1));
    assert_eq!(node.status(fixture.late.1).await.unwrap(), Value::Null);
    node.canonicalize(late_block).await?;
    let late = node.status(fixture.late.1).await.unwrap();
    assert_eq!(late["blockHash"], json!(late_block));
    assert_eq!(late["blockNumber"], "0x3");
    assert_eq!(late["isValidityTransaction"], true);

    Ok(block)
}

#[tokio::test(flavor = "multi_thread")]
async fn basic_builder_serves_recorded_denim_validity_status() -> eyre::Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();
    let root = tempfile::tempdir()?;
    let store = ValidityMetadataStore::open(root.path().join("validity.redb"))?;
    let enabled = ValidityNode::launch(
        MultiplexingServiceBuilder::new(builder_config())
            .with_basic_only(true)
            .with_validity_metadata(spawn_recorder(&store)),
        Some(store.clone()),
        &root.path().join("enabled"),
    )
    .await?;
    let enabled_block = assert_recorded_classification(&enabled, &store).await?;

    // Disabled metadata is behavior-neutral: the same inputs build the same block, and its
    // canonical transactions report unavailable metadata rather than `false`.
    let disabled = ValidityNode::launch(
        MultiplexingServiceBuilder::new(builder_config()).with_basic_only(true),
        None,
        &root.path().join("disabled"),
    )
    .await?;
    let fixture = Fixture::new();
    fixture.submit_initial(&disabled).await?;
    let disabled_block = disabled.build_block(false, vec![], true).await?;
    assert_eq!(disabled_block, enabled_block, "recording must not change the built block");
    assert_unavailable(disabled.status(fixture.matched.1).await);
    assert_unavailable(disabled.status(fixture.plain.1).await);

    // Node runtimes are leaked like the smoke test to avoid teardown races.
    std::mem::forget(enabled);
    std::mem::forget(disabled);
    std::mem::forget(root);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cutover_builder_serves_recorded_denim_validity_status() -> eyre::Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();
    let root = tempfile::tempdir()?;
    let store = ValidityMetadataStore::open(root.path().join("validity.redb"))?;
    let node = ValidityNode::launch(
        MultiplexingServiceBuilder::new(builder_config())
            .with_cutover_enabled(true)
            .with_validity_metadata(spawn_recorder(&store)),
        Some(store.clone()),
        &root.path().join("cutover"),
    )
    .await?;
    assert_recorded_classification(&node, &store).await?;

    std::mem::forget(node);
    std::mem::forget(root);
    Ok(())
}
