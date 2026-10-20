//! The unified binary drives its embedded execution node in memory: Engine API calls go straight
//! to the typed handler, and other requests are dispatched as JSON-RPC into the captured auth
//! module. This launches that node and drives one forkchoice, payload, and insertion cycle
//! through [`EngineEndpoint::InMemory`].

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B64, B256, U64};
use alloy_provider::{Provider, RootProvider, transport::TransportResult};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_engine::{
    ForkchoiceState, ForkchoiceUpdated, INVALID_FORK_CHOICE_STATE_ERROR, JwtSecret,
    PayloadAttributes,
};
use base_common_consensus::JovianExtraData;
use base_common_genesis::RollupConfig;
use base_common_network::{Base, BaseEngineApi};
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_consensus_engine::{EngineClient, EngineClientBuilder, EngineEndpoint, InMemoryTransport};
use base_consensus_providers::L1_RPC_TIMEOUT;
use base_execution_chainspec::BaseChainSpec;
use base_node_core::args::RollupArgs;
use base_node_runner::{BaseNodeRunner, test_utils::BLOCK_TIME_SECONDS};
use base_test_utils::build_test_genesis;
use eyre::{Result, WrapErr, eyre};
use jsonrpsee::types::error::INVALID_PARAMS_CODE;
use reth_db::{ClientVersion, init_db, mdbx::DatabaseArguments, test_utils::tempdir_path};
use reth_node_builder::{NodeBuilder, NodeConfig};
use reth_node_core::{
    args::{DatadirArgs, DiscoveryArgs, NetworkArgs, RpcServerArgs},
    dirs::{DataDirPath, MaybePlatformPath},
};
use reth_tasks::Runtime;
use serde_json::json;

/// Seals one block through the in-memory Engine API and reads it back over both transports.
#[tokio::test(flavor = "multi_thread")]
async fn in_memory_engine_seals_one_block() -> Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();

    let mut genesis = build_test_genesis();
    let base_fee_params = BaseChainSpec::from_genesis(genesis.clone())
        .base_fee_params_at_timestamp(genesis.timestamp);
    // Jovian is active at genesis, so a child of genesis that misses reth's built-payload cache
    // is validated against Jovian-encoded parent extra data.
    genesis.extra_data = JovianExtraData::encode(B64::ZERO, base_fee_params, 0)?;
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));
    let mut rpc = RpcServerArgs::default().with_unused_ports().with_http();
    rpc.http_api = Some("eth,miner".parse().wrap_err("Failed to parse HTTP API modules")?);
    let network = NetworkArgs {
        discovery: DiscoveryArgs { disable_discovery: true, ..DiscoveryArgs::default() },
        ..NetworkArgs::default()
    };
    let mut node_config =
        NodeConfig::new(chain_spec).with_network(network).with_rpc(rpc).with_unused_ports();
    // The next payload's parent header is read back from the database.
    node_config.engine.persistence_threshold = 0;
    let data_path = tempdir_path();
    node_config = node_config.with_datadir_args(DatadirArgs {
        datadir: MaybePlatformPath::<DataDirPath>::from(data_path.clone()),
        ..Default::default()
    });
    let db = init_db(
        node_config.datadir().db(),
        DatabaseArguments::new(ClientVersion::default())
            .with_geometry_max_size(Some(100 * 1024 * 1024)),
    )
    .wrap_err("Failed to create test database")?;

    let runtime = Runtime::test();
    let launched = BaseNodeRunner::new(RollupArgs::default())
        .launch(
            NodeBuilder::new(node_config).with_database(db).with_launch_context(runtime.clone()),
        )
        .await?;
    let http_addr = launched
        .handle
        .node
        .rpc_server_handle()
        .http_local_addr()
        .ok_or_else(|| eyre!("HTTP RPC server failed to bind"))?;
    let endpoint = EngineEndpoint::InMemory {
        transport: InMemoryTransport { module: launched.auth_module.clone() },
        engine_api: launched.engine_api.clone(),
    };
    let reads: RootProvider<Base> = endpoint.clone().connect(JwtSecret::random()).await?;
    let engine = EngineClientBuilder {
        l2: endpoint,
        l2_jwt: JwtSecret::random(),
        l1_rpc: "http://127.0.0.1:8545".parse()?,
        l1_rpc_timeout: L1_RPC_TIMEOUT,
        cfg: Arc::new(RollupConfig::default()),
    }
    .build()
    .await?;

    // `miner_*` is merged into the auth module inside `launch_add_ons_with`, before the runner
    // captures the module. A method-not-found here means that capture ran too early.
    let updated: bool =
        reads.client().request("miner_setMaxDASize", json!(["0x10", "0x20"])).await?;
    assert!(updated);
    let limits: (U64, U64) = reads.client().request("miner_getMaxDASize", json!([])).await?;
    assert_eq!(limits, (U64::from(0x10), U64::from(0x20)));

    let parent = engine
        .l2_block_by_label(BlockNumberOrTag::Latest)
        .await?
        .ok_or_else(|| eyre!("missing genesis block"))?;
    let parent_hash = parent.header.hash;
    let attributes = BasePayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp: parent.header.timestamp + BLOCK_TIME_SECONDS,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: parent.header.beneficiary,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(
                parent.header.parent_beacon_block_root.unwrap_or(B256::ZERO),
            ),
            slot_number: None,
            target_gas_limit: None,
        },
        transactions: Some(vec![]),
        no_tx_pool: Some(true),
        gas_limit: Some(parent.header.gas_limit),
        eip_1559_params: Some(B64::from(
            ((base_fee_params.max_change_denominator as u64) << 32)
                | base_fee_params.elasticity_multiplier as u64,
        )),
        min_base_fee: parent.header.base_fee_per_gas,
    };
    let started = engine
        .fork_choice_updated_v3(
            ForkchoiceState {
                head_block_hash: parent_hash,
                safe_block_hash: parent_hash,
                finalized_block_hash: parent_hash,
            },
            Some(attributes.clone()),
        )
        .await?;
    let payload_id =
        started.payload_id.ok_or_else(|| eyre!("forkchoice did not start a payload"))?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let envelope = engine.get_payload_v4(payload_id).await?;
    let status = engine
        .new_payload_v4(envelope.execution_payload, envelope.parent_beacon_block_root)
        .await?;
    assert!(status.status.is_valid(), "payload rejected: {status:?}");
    let hash = status.latest_valid_hash.ok_or_else(|| eyre!("payload status missing hash"))?;
    engine
        .fork_choice_updated_v3(
            ForkchoiceState {
                head_block_hash: hash,
                safe_block_hash: parent_hash,
                finalized_block_hash: parent_hash,
            },
            None,
        )
        .await?;

    let public = http_provider(http_addr)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if is_head(&reads, hash).await? && is_head(&public, hash).await? {
            break;
        }
        if Instant::now() >= deadline {
            return Err(eyre!("canonical head did not become the in-memory payload"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Consensus resets on `INVALID_FORK_CHOICE_STATE_ERROR`, so the typed path must surface
    // handler errors exactly as a JSON-RPC client would see them.
    let unknown_safe = ForkchoiceState {
        head_block_hash: hash,
        safe_block_hash: B256::repeat_byte(0x11),
        finalized_block_hash: parent_hash,
    };
    let (typed, json) = fcu_errors(&engine, &reads, unknown_safe, None).await?;
    assert_eq!(typed.0, INVALID_FORK_CHOICE_STATE_ERROR as i64);
    assert_eq!(typed, json);

    let canonical = ForkchoiceState { safe_block_hash: parent_hash, ..unknown_safe };
    let unsupported = BasePayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp: attributes.payload_attributes.timestamp + BLOCK_TIME_SECONDS,
            target_gas_limit: Some(parent.header.gas_limit),
            ..attributes.payload_attributes.clone()
        },
        ..attributes
    };
    let (typed, json) = fcu_errors(&engine, &reads, canonical, Some(unsupported)).await?;
    assert_eq!(typed.0, i64::from(INVALID_PARAMS_CODE));
    assert_eq!((typed.0, &typed.1), (json.0, &json.1));

    drop(launched);
    drop(runtime);
    let _ = std::fs::remove_dir_all(data_path);
    Ok(())
}

/// Error response code, message, and data.
type ErrorResp = (i64, String, Option<String>);

/// Sends one failing `engine_forkchoiceUpdatedV3` through the typed client and as JSON-RPC.
async fn fcu_errors(
    engine: &impl EngineClient,
    reads: &RootProvider<Base>,
    state: ForkchoiceState,
    attributes: Option<BasePayloadAttributes>,
) -> Result<(ErrorResp, ErrorResp)> {
    let typed = engine.fork_choice_updated_v3(state, attributes.clone()).await;
    let json = reads
        .client()
        .request::<_, ForkchoiceUpdated>("engine_forkchoiceUpdatedV3", (state, attributes))
        .await;
    Ok((error_resp(typed)?, error_resp(json)?))
}

fn error_resp(result: TransportResult<ForkchoiceUpdated>) -> Result<ErrorResp> {
    let error = result.err().ok_or_else(|| eyre!("forkchoice update unexpectedly succeeded"))?;
    let payload = error.as_error_resp().ok_or_else(|| eyre!("not an error response: {error}"))?;
    Ok((
        payload.code,
        payload.message.to_string(),
        payload.data.as_ref().map(|data| data.get().to_owned()),
    ))
}

fn http_provider(addr: SocketAddr) -> Result<RootProvider<Base>> {
    let client = RpcClient::builder().http(format!("http://{addr}").parse()?);
    Ok(RootProvider::new(client))
}

async fn is_head(provider: &RootProvider<Base>, hash: B256) -> Result<bool> {
    Ok(provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .is_some_and(|head| head.header.number == 1 && head.header.hash == hash))
}
