//! Real-node coverage for the unified sequencer's native-builder frozen-payload channel.
//!
//! These tests use the production runner and payload service, MDBX, the real transaction pool
//! and EVM, HTTP transaction submission, and the typed Engine API. The test drives the CL's
//! FCU/getPayload/newPayload sequence; L1 derivation, conductor, and P2P gossip are not started.

use std::{sync::Arc, time::Duration};

use alloy_eips::{BlockNumberOrTag, Encodable2718};
use alloy_network::ReceiptResponse;
use alloy_primitives::{B64, B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes, PayloadId};
use base_builder_core::{BuilderConfig, NativePayloadServiceBuilder};
use base_common_consensus::JovianExtraData;
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionRequest;
use base_common_rpc_types_engine::{
    BaseExecutionPayloadEnvelopeV4, BasePayloadAttributes, PayloadReady,
};
use base_execution_chainspec::BaseChainSpec;
use base_node_core::{TestNodeRuntime, args::RollupArgs};
use base_node_runner::{BaseNodeRunner, LaunchedBaseNode, test_utils::L1_BLOCK_INFO_DEPOSIT_TX};
use base_protocol::BaseTimeUpdateTx;
use base_test_utils::{Account, build_test_genesis};
use eyre::{Result, eyre};
use reth_db::{ClientVersion, init_db, mdbx::DatabaseArguments};
use reth_node_builder::{NodeBuilder, NodeConfig};
use reth_node_core::args::{DatadirArgs, NetworkArgs, RpcServerArgs};
use serde_json::json;
use tempfile::TempDir;
use tokio::{sync::broadcast::error::TryRecvError, time::timeout};

const TEST_TIMEOUT: Duration = Duration::from_secs(30);
const RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// Owns an actual execution node, including the production native payload service.
struct NativeNode {
    launched: LaunchedBaseNode,
    rpc: RootProvider<Base>,
    chain_spec: Arc<BaseChainSpec>,
    runtime: TestNodeRuntime,
    _datadir: TempDir,
}

impl NativeNode {
    async fn launch(denim: bool) -> Result<Self> {
        base_node_runner::test_utils::init_silenced_tracing();
        let mut genesis = build_test_genesis();
        if denim {
            genesis.config.extra_fields.insert("base".into(), json!({ "denim": 0 }));
        }
        let params = BaseChainSpec::from_genesis(genesis.clone())
            .base_fee_params_at_timestamp(genesis.timestamp);
        genesis.extra_data = JovianExtraData::encode(B64::ZERO, params, 0)?;
        let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));
        let datadir = tempfile::tempdir()?;
        let mut network = NetworkArgs::default().with_unused_ports();
        network.discovery.disable_discovery = true;
        let mut rpc = RpcServerArgs::default().with_unused_ports().with_http();
        rpc.http_api = Some("eth".parse()?);
        let config = NodeConfig::new(Arc::clone(&chain_spec))
            .with_network(network)
            .with_rpc(rpc)
            .with_unused_ports()
            .with_datadir_args(DatadirArgs {
                datadir: datadir.path().to_string_lossy().parse()?,
                ..Default::default()
            });
        let db = init_db(
            config.datadir().db(),
            DatabaseArguments::new(ClientVersion::default())
                .with_geometry_max_size(Some(100 * 1024 * 1024)),
        )?;
        let mut runtime = TestNodeRuntime::default();
        runtime.rocksdb_path = Some(config.datadir().rocksdb());
        // This is the same service selection and runner handoff as `base sequencer`.
        // Do not attach a sender directly to the builder: that would bypass the wiring under test.
        let launch = BaseNodeRunner::new(RollupArgs::default())
            .with_service_builder(NativePayloadServiceBuilder::new(BuilderConfig::default()))
            .launch(
                NodeBuilder::new(config)
                    .with_database(db)
                    .with_launch_context(runtime.runtime.clone()),
            );
        let launched = runtime.runtime.handle().spawn(launch).await??;
        let address = launched
            .handle
            .node
            .rpc_server_handle()
            .http_local_addr()
            .ok_or_else(|| eyre!("HTTP RPC did not bind"))?;
        let rpc =
            RootProvider::new(RpcClient::builder().http(format!("http://{address}").parse()?));
        Ok(Self { launched, rpc, chain_spec, runtime, _datadir: datadir })
    }

    /// Starts a real build from the canonical parent, with the scheduled system deposits.
    async fn start_build(&self, no_tx_pool: bool) -> Result<(PayloadReady, Vec<Bytes>)> {
        let parent = self
            .rpc
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await?
            .ok_or_else(|| eyre!("canonical parent missing"))?;
        let number = parent.header.number + 1;
        let mut deposits = vec![L1_BLOCK_INFO_DEPOSIT_TX];
        let timestamp = if let Some(schedule) = self.chain_spec.denim_timestamp_schedule()? {
            let (seconds, millis) = schedule.block_timestamp_parts(number);
            deposits
                .push(BaseTimeUpdateTx::new(millis)?.into_deposit_tx(number).encoded_2718().into());
            seconds
        } else {
            parent.header.timestamp + 2
        };
        let params = self.chain_spec.base_fee_params_at_timestamp(timestamp);
        let attributes = BasePayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: parent.header.beneficiary,
                withdrawals: Some(vec![]),
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
            transactions: Some(deposits.clone()),
            no_tx_pool: Some(no_tx_pool),
            gas_limit: Some(parent.header.gas_limit),
            eip_1559_params: Some(B64::from(
                ((params.max_change_denominator as u64) << 32)
                    | params.elasticity_multiplier as u64,
            )),
            min_base_fee: parent.header.base_fee_per_gas,
        };
        let parent_hash = parent.header.hash;
        let response = self
            .launched
            .engine_api
            .fork_choice_updated_v3(
                ForkchoiceState {
                    head_block_hash: parent_hash,
                    safe_block_hash: parent_hash,
                    finalized_block_hash: parent_hash,
                },
                Some(attributes),
            )
            .await?;
        assert!(response.payload_status.status.is_valid());
        let payload_id = response.payload_id.ok_or_else(|| eyre!("FCU did not start a build"))?;
        Ok((PayloadReady { payload_id, parent_hash }, deposits))
    }

    async fn ready(&mut self, expected: PayloadReady) -> Result<()> {
        assert_eq!(timeout(RPC_TIMEOUT, self.launched.payload_ready.recv()).await??, expected);
        Ok(())
    }

    async fn payload(&self, id: PayloadId) -> Result<BaseExecutionPayloadEnvelopeV4> {
        Ok(timeout(RPC_TIMEOUT, self.launched.engine_api.get_payload_v4(id)).await??)
    }

    /// Imports the retrieved payload and waits for its canonical RPC view, without fixed sleeps.
    async fn insert(&self, envelope: BaseExecutionPayloadEnvelopeV4) -> Result<B256> {
        let payload = &envelope.execution_payload.payload_inner.payload_inner.payload_inner;
        let hash = payload.block_hash;
        let parent_hash = payload.parent_hash;
        let status = self
            .launched
            .engine_api
            .new_payload_v4(envelope.execution_payload, envelope.parent_beacon_block_root)
            .await?;
        assert!(status.status.is_valid(), "retrieved payload rejected: {status:?}");
        let response = self
            .launched
            .engine_api
            .fork_choice_updated_v3(
                ForkchoiceState {
                    head_block_hash: hash,
                    safe_block_hash: parent_hash,
                    finalized_block_hash: parent_hash,
                },
                None,
            )
            .await?;
        assert!(response.payload_status.status.is_valid());
        timeout(RPC_TIMEOUT, async {
            loop {
                let block = self.rpc.get_block_by_number(BlockNumberOrTag::Latest).await?.unwrap();
                if block.header.hash == hash {
                    break;
                }
                tokio::task::yield_now().await;
            }
            Ok::<_, eyre::Report>(())
        })
        .await??;
        Ok(hash)
    }

    async fn shutdown(self) -> Result<()> {
        timeout(RPC_TIMEOUT, self.runtime.runtime.initiate_graceful_shutdown()?.ignore_guard())
            .await?;
        timeout(RPC_TIMEOUT, self.launched.handle.node_exit_future).await??;
        Ok(())
    }
}

/// Signs a deterministic, funded ETH transfer rather than fabricating a pool entry or receipt.
fn transfer(nonce: u64, value: u64) -> Result<(Bytes, B256)> {
    Account::Alice.sign_txn_request(
        BaseTransactionRequest::default()
            .to(Account::Bob.address())
            .nonce(nonce)
            .value(U256::from(value)),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn native_frozen_payload_includes_pool_tx_and_defers_post_freeze_arrival() -> Result<()> {
    timeout(TEST_TIMEOUT, async {
        let mut node = NativeNode::launch(true).await?;
        let initial_balance = node.rpc.get_balance(Account::Bob.address()).await?;
        let (first_raw, first_hash) = transfer(0, 123)?;
        let pending = node.rpc.send_raw_transaction(&first_raw).await?;
        assert_eq!(*pending.tx_hash(), first_hash);
        let (first_ready, mut expected_transactions) = node.start_build(false).await?;
        node.ready(first_ready).await?;

        // A transaction accepted after the ready signal must not alter the frozen block.
        let (late_raw, late_hash) = transfer(1, 456)?;
        let pending = node.rpc.send_raw_transaction(&late_raw).await?;
        assert_eq!(*pending.tx_hash(), late_hash);
        let first = node.payload(first_ready.payload_id).await?;
        expected_transactions.push(first_raw);
        assert_eq!(
            first.execution_payload.payload_inner.payload_inner.payload_inner.transactions,
            expected_transactions
        );
        assert_eq!(node.launched.payload_ready.try_recv(), Err(TryRecvError::Empty));
        let first_block_hash = node.insert(first).await?;
        let receipt =
            node.rpc.get_transaction_receipt(first_hash).await?.expect("first transfer receipt");
        assert!(receipt.status());
        assert_eq!(receipt.block_hash(), Some(first_block_hash));
        assert_eq!(
            node.rpc.get_balance(Account::Bob.address()).await?,
            initial_balance + U256::from(123)
        );
        assert_eq!(node.rpc.get_transaction_count(Account::Alice.address()).await?, 1);
        assert!(node.rpc.get_transaction_receipt(late_hash).await?.is_none());

        let (next_ready, mut expected_transactions) = node.start_build(false).await?;
        assert_eq!(next_ready.parent_hash, first_block_hash);
        assert_ne!(next_ready.payload_id, first_ready.payload_id);
        node.ready(next_ready).await?;
        let second = node.payload(next_ready.payload_id).await?;
        expected_transactions.push(late_raw);
        assert_eq!(
            second.execution_payload.payload_inner.payload_inner.payload_inner.transactions,
            expected_transactions
        );
        let second_block_hash = node.insert(second).await?;
        let receipt =
            node.rpc.get_transaction_receipt(late_hash).await?.expect("late transfer receipt");
        assert!(receipt.status());
        assert_eq!(receipt.block_hash(), Some(second_block_hash));
        assert_eq!(
            node.rpc.get_balance(Account::Bob.address()).await?,
            initial_balance + U256::from(579)
        );
        assert_eq!(node.rpc.get_transaction_count(Account::Alice.address()).await?, 2);
        assert_eq!(node.launched.payload_ready.try_recv(), Err(TryRecvError::Empty));
        node.shutdown().await
    })
    .await?
}

#[tokio::test(flavor = "multi_thread")]
async fn native_replay_ignores_pool_without_signalling_ready() -> Result<()> {
    timeout(TEST_TIMEOUT, async {
        let mut node = NativeNode::launch(true).await?;
        let initial_balance = node.rpc.get_balance(Account::Bob.address()).await?;
        let (raw, hash) = transfer(0, 123)?;
        let pending = node.rpc.send_raw_transaction(&raw).await?;
        assert_eq!(*pending.tx_hash(), hash);
        let (ready, deposits) = node.start_build(true).await?;
        // getPayload joins the real build. Any notification would be sent before it returns,
        // so absence is checked without a sleep or a race with an unfinished build.
        let payload = node.payload(ready.payload_id).await?;
        assert_eq!(
            payload.execution_payload.payload_inner.payload_inner.payload_inner.transactions,
            deposits
        );
        assert_eq!(node.launched.payload_ready.try_recv(), Err(TryRecvError::Empty));
        node.insert(payload).await?;
        assert!(node.rpc.get_transaction_receipt(hash).await?.is_none());
        assert_eq!(node.rpc.get_balance(Account::Bob.address()).await?, initial_balance);
        node.shutdown().await
    })
    .await?
}

#[tokio::test(flavor = "multi_thread")]
async fn native_pre_denim_build_includes_pool_without_signalling_ready() -> Result<()> {
    timeout(TEST_TIMEOUT, async {
        let mut node = NativeNode::launch(false).await?;
        let (raw, hash) = transfer(0, 123)?;
        let pending = node.rpc.send_raw_transaction(&raw).await?;
        assert_eq!(*pending.tx_hash(), hash);
        let (ready, mut expected_transactions) = node.start_build(false).await?;
        let payload = node.payload(ready.payload_id).await?;
        expected_transactions.push(raw);
        assert_eq!(
            payload.execution_payload.payload_inner.payload_inner.payload_inner.transactions,
            expected_transactions
        );
        assert_eq!(node.launched.payload_ready.try_recv(), Err(TryRecvError::Empty));
        let block_hash = node.insert(payload).await?;
        let receipt =
            node.rpc.get_transaction_receipt(hash).await?.expect("legacy transfer receipt");
        assert!(receipt.status());
        assert_eq!(receipt.block_hash(), Some(block_hash));
        node.shutdown().await
    })
    .await?
}
