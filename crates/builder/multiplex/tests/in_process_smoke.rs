//! In-process integration smoke test for multiplex payload routing.

use std::{net::TcpListener, sync::Arc, time::Duration};

use alloy_eips::{BlockNumberOrTag, Encodable2718};
use alloy_primitives::{B64, B256};
use alloy_provider::{Identity, Provider, ProviderBuilder, RootProvider};
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes};
use base_builder_core::{BuilderConfig, FlashblocksServiceBuilder};
use base_builder_multiplex::MultiplexingServiceBuilder;
use base_common_chains::Upgrades;
use base_common_consensus::BaseTxEnvelope;
use base_common_genesis::{BaseUpgrade, RollupConfig};
use base_common_network::Base;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_payload_builder::BasePayloadBuilderAttributes;
use base_execution_rpc::BaseEngineApiClient;
use base_node_core::{BaseEngineTypes, args::RollupArgs};
use base_node_runner::BaseNodeRunner;
use base_protocol::{BaseTimeUpdateTx, L1BlockInfoTx};
use futures::StreamExt;
use reth_ethereum_forks::ForkCondition;
use reth_node_api::{EngineTypes, PayloadTypes};
use reth_node_builder::NodeBuilder;
use reth_payload_builder::PayloadId;
use reth_tasks::{Runtime, RuntimeBuilder, RuntimeConfig};
use tokio_tungstenite::connect_async;

#[derive(Debug)]
struct RunningNode {
    auth_ipc_path: String,
    rpc_ipc_path: String,
    chain_spec: Arc<BaseChainSpec>,
    _runtime: Runtime,
    _handle: base_node_runner::LaunchedBaseNode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BuiltBlockSummary {
    hash: B256,
    state_root: B256,
    gas_used: u64,
    tx_count: usize,
}

impl RunningNode {
    async fn launch_flashblocks() -> eyre::Result<Self> {
        Self::launch(
            FlashblocksServiceBuilder::new(test_builder_config()),
            BaseChainSpec::mainnet(),
        )
        .await
    }

    async fn launch_multiplex() -> eyre::Result<Self> {
        let service_builder = MultiplexingServiceBuilder::new(test_builder_config());
        Self::launch(service_builder, BaseChainSpec::mainnet()).await
    }

    async fn launch_basic() -> eyre::Result<Self> {
        let service_builder =
            MultiplexingServiceBuilder::new(test_builder_config()).with_basic_only(true);
        Self::launch(service_builder, BaseChainSpec::mainnet()).await
    }

    async fn launch<SB>(service_builder: SB, chain_spec: BaseChainSpec) -> eyre::Result<Self>
    where
        SB: base_node_runner::PayloadServiceBuilder,
    {
        let runtime = RuntimeBuilder::new(RuntimeConfig::default()).build()?;
        let chain_spec = Arc::new(chain_spec);
        let mut node_config =
            reth_node_builder::NodeConfig::new(Arc::clone(&chain_spec)).with_unused_ports();
        node_config.rpc = reth_node_core::args::RpcServerArgs::default().with_auth_ipc();
        node_config.rpc.http = false;
        node_config.rpc.ws = false;
        node_config.rpc.auth_port = 0;

        let random_id = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos()
        );
        let data_path = std::env::temp_dir().join(format!("mux-test.{random_id}.datadir"));
        let rocksdb_path = std::env::temp_dir().join(format!("mux-test.{random_id}.rocksdb"));
        let pprof_path = std::env::temp_dir().join(format!("mux-test.{random_id}.pprof-dumps"));
        std::fs::create_dir_all(&data_path)?;
        std::fs::create_dir_all(&rocksdb_path)?;
        std::fs::create_dir_all(&pprof_path)?;
        node_config.rpc.ipcpath = data_path.join("rpc.ipc").to_string_lossy().into_owned();
        node_config.rpc.auth_ipc_path = data_path.join("engine.ipc").to_string_lossy().into_owned();
        node_config = node_config.with_datadir_args(reth_node_core::args::DatadirArgs {
            datadir: data_path.to_string_lossy().parse()?,
            static_files_path: None,
            rocksdb_path: Some(rocksdb_path),
            pprof_dumps_path: Some(pprof_path),
        });

        let db_root = std::env::temp_dir().join(format!(
            "mux-test-db-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos()
        ));
        std::fs::create_dir_all(&db_root)?;
        let db = reth_db::init_db(
            db_root.as_path(),
            reth_db::mdbx::DatabaseArguments::new(reth_db::ClientVersion::default()),
        )?;

        let runner =
            BaseNodeRunner::new(RollupArgs::default()).with_service_builder(service_builder);
        let builder = NodeBuilder::new(node_config.clone())
            .with_database(db)
            .with_launch_context(runtime.clone());
        let handle = runner.launch(builder).await?;

        Ok(Self {
            auth_ipc_path: node_config.rpc.auth_ipc_path,
            rpc_ipc_path: node_config.rpc.ipcpath,
            chain_spec,
            _runtime: runtime,
            _handle: handle,
        })
    }

    async fn provider(&self) -> eyre::Result<RootProvider<Base>> {
        ProviderBuilder::<Identity, Identity, Base>::default()
            .connect_ipc(self.rpc_ipc_path.clone().into())
            .await
            .map_err(|err| eyre::eyre!("Failed to connect to IPC provider: {err}"))
    }
}

fn test_builder_config() -> BuilderConfig {
    let mut config = BuilderConfig::default();
    config.flashblocks_ws_addr.set_port(available_port());
    config
}

fn available_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("failed to bind random local port")
        .local_addr()
        .expect("failed to get local listener addr")
        .port()
}

async fn engine_forkchoice_updated(
    auth_ipc_path: &str,
    state: ForkchoiceState,
    attrs: Option<<BaseEngineTypes as PayloadTypes>::PayloadAttributes>,
) -> eyre::Result<alloy_rpc_types_engine::ForkchoiceUpdated> {
    let client = reth_ipc::client::IpcClientBuilder::default().build(auth_ipc_path).await?;
    Ok(BaseEngineApiClient::<BaseEngineTypes>::fork_choice_updated_v3(&client, state, attrs)
        .await?)
}

async fn engine_get_payload(
    auth_ipc_path: &str,
    payload_id: PayloadId,
) -> eyre::Result<<BaseEngineTypes as EngineTypes>::ExecutionPayloadEnvelopeV3> {
    let client = reth_ipc::client::IpcClientBuilder::default().build(auth_ipc_path).await?;
    Ok(BaseEngineApiClient::<BaseEngineTypes>::get_payload_v3(&client, payload_id).await?)
}

async fn build_new_block(
    provider: &RootProvider<Base>,
    node: &RunningNode,
    block_timestamp: u64,
    no_tx_pool: bool,
) -> eyre::Result<BuiltBlockSummary> {
    let latest = provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .ok_or_else(|| eyre::eyre!("missing latest block"))?;

    let parent_hash = latest.header.hash;
    let parent_beacon_block_root = latest.header.parent_beacon_block_root.unwrap_or(B256::ZERO);
    let eip_1559_params: u64 = ((50_u64) << 32) | 2_u64;

    let mut transactions = vec![];
    if node.chain_spec.is_denim_active_at_timestamp(block_timestamp) {
        // FCU validates the mandatory BaseTime deposit even in this isolated builder smoke test.
        // The mainnet genesis fixture has the Bedrock L1Block contract.
        let mut rollup_config = RollupConfig::default();
        rollup_config.upgrades.regolith_time = Some(0);
        let l1_info = L1BlockInfoTx::Bedrock(Default::default())
            .into_deposit_tx(&rollup_config, block_timestamp);
        let base_time = BaseTimeUpdateTx::new(0)?.into_deposit_tx(latest.header.number + 1);
        transactions.push(l1_info.encoded_2718().into());
        transactions.push(base_time.encoded_2718().into());
    }

    let payload_attributes = BasePayloadBuilderAttributes::<BaseTxEnvelope>::try_new(
        parent_hash,
        BasePayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: block_timestamp,
                parent_beacon_block_root: Some(parent_beacon_block_root),
                withdrawals: Some(vec![]),
                slot_number: None,
                ..Default::default()
            },
            transactions: Some(transactions),
            gas_limit: Some(10_000_000),
            no_tx_pool: Some(no_tx_pool),
            min_base_fee: Some(0),
            eip_1559_params: Some(B64::from(eip_1559_params)),
        },
        3,
    )?;

    let fcu = engine_forkchoice_updated(
        &node.auth_ipc_path,
        ForkchoiceState {
            head_block_hash: parent_hash,
            safe_block_hash: parent_hash,
            finalized_block_hash: parent_hash,
        },
        Some(payload_attributes),
    )
    .await?;

    let payload_id = fcu.payload_id.ok_or_else(|| eyre::eyre!("fcu did not return payload id"))?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let payload = engine_get_payload(&node.auth_ipc_path, payload_id).await?.execution_payload;
    let repeated_payload =
        engine_get_payload(&node.auth_ipc_path, payload_id).await?.execution_payload;
    assert_eq!(
        payload.payload_inner.payload_inner.block_hash,
        repeated_payload.payload_inner.payload_inner.block_hash,
        "repeated getPayload returned a different block"
    );
    let payload = payload.payload_inner.payload_inner;

    Ok(BuiltBlockSummary {
        hash: payload.block_hash,
        state_root: payload.state_root,
        gas_used: payload.gas_used,
        tx_count: payload.transactions.len(),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn payload_builder_modes_match_flashblocks_baseline() -> eyre::Result<()> {
    let baseline = RunningNode::launch_flashblocks().await?;
    let baseline_provider = baseline.provider().await?;

    let multiplex = RunningNode::launch_multiplex().await?;
    let multiplex_provider = multiplex.provider().await?;

    let baseline_head = baseline_provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .ok_or_else(|| eyre::eyre!("baseline missing latest block"))?;
    let multiplex_head = multiplex_provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .ok_or_else(|| eyre::eyre!("multiplex missing latest block"))?;

    assert_eq!(baseline_head.header.hash, multiplex_head.header.hash, "genesis mismatch");

    let wall_clock =
        std::time::SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH)?;
    let build_timestamp =
        std::cmp::max(baseline_head.header.timestamp + 2, wall_clock.as_secs() + 2);
    let baseline_block =
        build_new_block(&baseline_provider, &baseline, build_timestamp, true).await?;
    let multiplex_block =
        build_new_block(&multiplex_provider, &multiplex, build_timestamp, true).await?;

    assert_eq!(baseline_block, multiplex_block);

    let basic = RunningNode::launch_basic().await?;
    let basic_provider = basic.provider().await?;
    let basic_block = build_new_block(&basic_provider, &basic, build_timestamp, true).await?;
    assert_eq!(baseline_block, basic_block);

    // Keep the nodes alive until process exit; dropping their independent runtimes can race the
    // remaining providers and database tasks during test teardown.
    std::mem::forget(baseline);
    std::mem::forget(multiplex);
    std::mem::forget(basic);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn default_builder_starts_with_scheduled_native_upgrade() -> eyre::Result<()> {
    for upgrade in [BaseUpgrade::Cobalt, BaseUpgrade::Denim, BaseUpgrade::Zenith] {
        for timestamp in [0, u64::MAX] {
            let chain_spec = BaseChainSpecBuilder::base_mainnet()
                .with_fork(upgrade, ForkCondition::Timestamp(timestamp))
                .build();
            let config = test_builder_config();
            let url = format!("ws://127.0.0.1:{}", config.flashblocks_ws_addr.port());
            let node =
                RunningNode::launch(MultiplexingServiceBuilder::new(config), chain_spec).await?;
            // The Flashblocks service remains available even when already retired at startup.
            let (mut stream, _) = connect_async(url).await?;
            let provider = node.provider().await?;
            let build_timestamp =
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() + 2;
            let block = build_new_block(&provider, &node, build_timestamp, false).await?;
            assert_ne!(block.hash, B256::ZERO);
            let message = tokio::time::timeout(Duration::from_millis(500), stream.next()).await;
            if timestamp == 0 {
                assert!(message.is_err(), "Flashblocks published after {upgrade:?}");
            } else {
                assert!(
                    message.expect("pre-fork Flashblocks message").expect("open stream")?.is_text(),
                    "expected a pre-fork Flashblocks payload"
                );
            }
            std::mem::forget(node);
        }
    }
    Ok(())
}
