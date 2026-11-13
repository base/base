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
    seals_one_block(false).await
}

/// Launch admission, actual native build, independent newPayload and FCU work together.
#[tokio::test(flavor = "multi_thread")]
async fn deferred_in_memory_engine_seals_and_accepts_one_block() -> Result<()> {
    seals_one_block(true).await
}

/// Authenticated but uncertain snapshots must reject before any writable store open.
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_full_launch_rejection_preserves_all_store_files() -> Result<()> {
    use base_node_runner::{DeferredStateRootAdmission, DeferredStateRootSnapshot};
    use reth_provider::{ProviderFactory, providers::{RocksDBProvider, StaticFileProviderBuilder}, BlockWriter, ChainStateBlockWriter, StageCheckpointWriter};
    use reth_node_builder::NodeTypesWithDBAdapter;
    use reth_primitives_traits::Block;
    for active in [false, true] {
      for early in [false, true] {
        let directory = tempfile::tempdir()?;
        let mut genesis = build_test_genesis();
        let activation = genesis.timestamp + BLOCK_TIME_SECONDS;
        genesis.config.extra_fields.insert("base".into(), json!({"deferredStateRoot": activation}));
        // The active case deliberately presents an authenticated block at activation.
        // The uncertain case instead has safe metadata beyond its durable tip.
        let chain = Arc::new(BaseChainSpec::from_genesis(genesis));
        let mut config = NodeConfig::new(chain.clone()).with_unused_ports().with_datadir_args(DatadirArgs { datadir: directory.path().join("run1").into(), ..Default::default() });
        config.debug.invalid_block_hook = None;
        let runtime = Runtime::test();
        let db = init_db(config.datadir().db(), DatabaseArguments::new(ClientVersion::default()).with_geometry_max_size(Some(100 * 1024 * 1024)))?;
        let factory = ProviderFactory::<NodeTypesWithDBAdapter<base_node_runner::BaseNode, _>>::new(db, chain.clone(),
            StaticFileProviderBuilder::read_write(config.datadir().static_files()).build()?,
            RocksDBProvider::builder(config.datadir().rocksdb()).with_default_tables().build()?, runtime.clone())?;
        reth_db_common::init::init_genesis(&factory)?;
        let writer = factory.provider_rw()?;
        let (tip_number, tip_hash) = if active {
            let mut header = chain.genesis_header().clone(); header.number = 1; header.timestamp = activation; header.parent_hash = chain.genesis_hash();
            let block = base_common_consensus::BaseBlock::new(header, Default::default()).try_into_recovered()?;
            let hash = block.hash(); writer.insert_block(&block)?;
            writer.save_stage_checkpoint(reth_stages_types::StageId::Finish, reth_stages_types::StageCheckpoint::new(1))?;
            (1, hash)
        } else { writer.save_safe_block_number(1)?; (0, chain.genesis_hash()) };
        writer.commit()?;
        drop(factory);
        let files = DeferredStateRootAdmission::store_digests(config.datadir().data_dir())?;
        let lock_names = [config.datadir().db().join("mdbx.lck"), config.datadir().rocksdb().join("LOCK")].map(|path| (path.clone(), path.exists()));
        assert!(files.keys().any(|path| path.starts_with("rocksdb/")), "fixture must contain actual persisted RocksDB files");
        let snapshot = DeferredStateRootSnapshot { provenance: "offline authenticated rejection fixture".into(), genesis_hash: chain.genesis_hash(), tip_hash, tip_number, files: files.clone() };
        let manifest = directory.path().join("snapshot.json"); std::fs::write(&manifest, serde_json::to_vec(&snapshot)?)?;
        let args = RollupArgs { deferred_state_root_disposable: true, deferred_state_root_snapshot: Some(manifest), ..Default::default() };
        let result = if early {
            base_node_runner::DeferredStateRootPrepared::prepare_database(&config, &args, runtime.clone()).map(|_| ())
        } else {
            let db = reth_db::open_db_read_only(config.datadir().db(), DatabaseArguments::new(ClientVersion::default()))?;
            BaseNodeRunner::new(args).launch(NodeBuilder::new(config.clone()).with_database(db).with_launch_context(runtime.clone())).await.map(|_| ())
        };
        assert!(result.is_err(), "authenticated active/uncertain snapshot must reject full launch");
        let error = result.err().unwrap().to_string();
        assert!(error.contains(if active { "already contains active history" } else { "safe/finalized metadata exceeds" }), "snapshot must reach semantic read-only rejection, not manifest/config failure: {error}");
        assert_eq!(DeferredStateRootAdmission::store_digests(config.datadir().data_dir())?, files, "full launch rejection must preserve every original MDBX/static/RocksDB byte and filename");
        for (path, existed) in lock_names { assert_eq!(path.exists(), existed, "rejection must preserve even the mutable OS lock filenames"); }
        assert!(runtime.graceful_shutdown_with_timeout(Duration::from_secs(10)), "rejection must leave no runtime work");
      }
    }
    Ok(())
}

/// Exercises identical node/Engine API plumbing with ordinary or configured deferred roots.
async fn seals_one_block(deferred: bool) -> Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();

    let mut genesis = build_test_genesis();
    let base_fee_params = BaseChainSpec::from_genesis(genesis.clone())
        .base_fee_params_at_timestamp(genesis.timestamp);
    // Jovian is active at genesis, so a child of genesis that misses reth's built-payload cache
    // is validated against Jovian-encoded parent extra data.
    genesis.extra_data = JovianExtraData::encode(B64::ZERO, base_fee_params, 0)?;
    if deferred {
        genesis.config.extra_fields.insert("base".into(), json!({"deferredStateRoot": genesis.timestamp + BLOCK_TIME_SECONDS}));
    }
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
    if deferred { node_config.debug.invalid_block_hook = None; }
    let data_path = tempdir_path();
    node_config = node_config.with_datadir_args(DatadirArgs {
        datadir: MaybePlatformPath::<DataDirPath>::from(data_path.clone()),
        ..Default::default()
    });
    // Construct outside Tokio so this node owns a runtime. An attached Runtime::test only
    // waits graceful guards, not the cancellation/destruction of every provider-owning task.
    let runtime = std::thread::spawn(Runtime::test).join().expect("node runtime construction must succeed");
    let mut replay_config = node_config.clone();
    let args = RollupArgs { deferred_state_root_disposable: deferred, ..Default::default() };
    let (db, prepared) = base_node_runner::DeferredStateRootPrepared::prepare_database(&node_config, &args, runtime.clone())?;
    let launched = BaseNodeRunner::new(args).with_prepared_root_admission(prepared)
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
    if deferred {
        assert_eq!(envelope.execution_payload.clone().into_block_raw()?.header.state_root, parent.header.state_root, "actual guarded runner getPayload F must commit exactly the real legacy parent root");
    }
    let mut bad_hash = envelope.execution_payload.clone();
    bad_hash.payload_inner.payload_inner.payload_inner.block_hash = B256::repeat_byte(0x44);
    let invalid_hash = engine.new_payload_v4(bad_hash, envelope.parent_beacon_block_root).await?;
    assert!(matches!(invalid_hash.status, alloy_rpc_types_engine::PayloadStatusEnum::Invalid { .. }), "ordinary/deferred Engine must reject a malformed payload blockHash: {invalid_hash:?}");
    assert_eq!(invalid_hash.latest_valid_hash, Some(parent_hash), "malformed blockHash must retain the pinned engine's exact known valid parent");
    let mut bad_withdrawals = envelope.execution_payload.clone();
    bad_withdrawals.withdrawals_root = B256::repeat_byte(0x45);
    bad_withdrawals.payload_inner.payload_inner.payload_inner.block_hash = bad_withdrawals.clone().into_block_raw()?.header.hash_slow();
    let invalid_withdrawals = engine.new_payload_v4(bad_withdrawals, envelope.parent_beacon_block_root).await?;
    assert!(matches!(invalid_withdrawals.status, alloy_rpc_types_engine::PayloadStatusEnum::Invalid { .. }), "same-block withdrawalsRoot validation must remain enabled in every cohort: {invalid_withdrawals:?}");
    assert_eq!(invalid_withdrawals.latest_valid_hash, Some(parent_hash), "wrong same-block message-passer root must identify exactly the valid parent");
    let repeated = envelope.execution_payload.clone();
    let status = engine
        .new_payload_v4(envelope.execution_payload, envelope.parent_beacon_block_root)
        .await?;
    assert!(status.status.is_valid(), "payload rejected: {status:?}");
    let hash = status.latest_valid_hash.ok_or_else(|| eyre!("payload status missing hash"))?;
    let duplicate = engine.new_payload_v4(repeated, envelope.parent_beacon_block_root).await?;
    assert_eq!(duplicate.status, alloy_rpc_types_engine::PayloadStatusEnum::Valid, "already checked exact payload hash must remain VALID on duplicate import");
    assert_eq!(duplicate.latest_valid_hash, Some(hash), "duplicate import must return the exact previously validated hash");
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
    if deferred {
        let proof = public.get_proof(alloy_primitives::Address::ZERO, vec![]).await;
        assert!(proof.is_err(), "configured deferred runner must fail closed for public proof RPC");
        assert!(proof.unwrap_err().to_string().contains("proof RPC is unsupported"), "proof RPC must fail at the compatibility guard rather than emit a misanchored proof");
    } else {
        let public_proof = public.get_proof(alloy_primitives::Address::ZERO, vec![B256::ZERO]).block_id(hash.into()).await?;
        let auth_proof = reads.get_proof(alloy_primitives::Address::ZERO, vec![B256::ZERO]).block_id(hash.into()).await?;
        assert_eq!(public_proof, auth_proof, "Never proof RPC must preserve exact account/storage proof bytes across public and in-memory transports");
        assert_eq!(public_proof.address, alloy_primitives::Address::ZERO, "proof must identify exactly the requested account");
        assert_eq!(public_proof.storage_proof.len(), 1, "proof must return exactly the one requested storage slot");
        assert_eq!(public_proof.storage_proof[0].value, alloy_primitives::U256::ZERO, "zero-account slot zero has exactly zero value in the fixture");
    }
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

    // In-memory RPC transports own the provider too; release them before reopening the snapshot.
    drop(engine); drop(reads); drop(public);
    drop(launched);
    assert!(runtime.graceful_shutdown_with_timeout(Duration::from_secs(10)), "runner exit must drain its owned current-root and redundant stock work");
    // Join the owned Tokio runtime before snapshotting: cancellation drops async provider
    // owners, including StaticFileProvider's StorageLock whose Drop deletes static_files/lock.
    tokio::task::spawn_blocking(move || runtime.shutdown_timeout(Duration::from_secs(10))).await?;
    if !deferred {
        // This ordinary persisted tip is an unmarked immutable pre-F snapshot, not an active restart.
        let mut genesis = replay_config.chain.genesis().clone();
        genesis.config.extra_fields.insert("base".into(), json!({"deferredStateRoot": parent.header.timestamp + 1000}));
        replay_config.chain = Arc::new(BaseChainSpec::from_genesis(genesis));
        replay_config.debug.invalid_block_hook = None;
        let restored = tempfile::tempdir()?;
        let destination = restored.path().join("run2");
        let original = replay_config.datadir();
        assert!(!original.static_files().join("lock").exists(), "offline snapshot requires the static-file writer and its lock to be released");
        let source_files = base_node_runner::DeferredStateRootAdmission::store_digests(original.data_dir())?;
        let mut pending = ["db", "static_files", "rocksdb"].map(|store| (original.data_dir().join(store), destination.join(store))).to_vec();
        while let Some((source, target)) = pending.pop() {
            if source.is_dir() {
                std::fs::create_dir_all(&target)?;
                for entry in std::fs::read_dir(source)? { let entry = entry?; pending.push((entry.path(), target.join(entry.file_name()))); }
            } else { std::fs::copy(&source, &target).wrap_err_with(|| format!("offline snapshot copy {} to {}", source.display(), target.display()))?; }
        }
        replay_config.datadir.datadir = destination.into();
        let files = base_node_runner::DeferredStateRootAdmission::store_digests(replay_config.datadir().data_dir())?;
        assert_eq!(files, source_files, "snapshot copy must preserve every immutable store filename and byte");
        assert_eq!(base_node_runner::DeferredStateRootAdmission::store_digests(original.data_dir())?, source_files, "offline source must remain unchanged throughout snapshot copy");
        let manifest = data_path.with_extension("snapshot.json");
        let snapshot = base_node_runner::DeferredStateRootSnapshot { provenance: "ordinary persisted block1 before activation".into(), genesis_hash: replay_config.chain.genesis_hash(), tip_hash: hash, tip_number: 1, files };
        std::fs::write(&manifest, serde_json::to_vec(&snapshot)?)?;
        let args = RollupArgs { deferred_state_root_disposable: true, deferred_state_root_snapshot: Some(manifest), ..Default::default() };
        let replay_runtime = Runtime::test();
        let (db, prepared) = base_node_runner::DeferredStateRootPrepared::prepare_database(&replay_config, &args, replay_runtime.clone())?;
        let replay = BaseNodeRunner::new(args).with_prepared_root_admission(prepared).launch(NodeBuilder::new(replay_config).with_database(db).with_launch_context(replay_runtime.clone())).await?;
        let reads = RootProvider::<Base>::new(RpcClient::new(InMemoryTransport { module: replay.auth_module.clone() }, false));
        let latest = reads.get_block_by_number(BlockNumberOrTag::Latest).await?.unwrap();
        assert_eq!(latest.header.hash, hash, "admitted immutable non-genesis pre-F snapshot must launch at exactly the persisted block");
        assert_eq!(latest.header.number, 1, "admitted snapshot must retain exactly its durable height");
        let endpoint = EngineEndpoint::InMemory { transport: InMemoryTransport { module: replay.auth_module.clone() }, engine_api: replay.engine_api.clone() };
        let replay_engine = EngineClientBuilder { l2: endpoint, l2_jwt: JwtSecret::random(), l1_rpc: "http://127.0.0.1:8545".parse()?, l1_rpc_timeout: L1_RPC_TIMEOUT, cfg: Arc::new(RollupConfig::default()) }.build().await?;
        let attributes = BasePayloadAttributes { payload_attributes: PayloadAttributes { timestamp: latest.header.timestamp + BLOCK_TIME_SECONDS, prev_randao: B256::ZERO, suggested_fee_recipient: latest.header.beneficiary, withdrawals: Some(vec![]), parent_beacon_block_root: Some(B256::ZERO), slot_number: None, target_gas_limit: None }, transactions: Some(vec![]), no_tx_pool: Some(true), gas_limit: Some(latest.header.gas_limit), eip_1559_params: Some(B64::from(((base_fee_params.max_change_denominator as u64) << 32) | base_fee_params.elasticity_multiplier as u64)), min_base_fee: latest.header.base_fee_per_gas };
        let parent_state = ForkchoiceState { head_block_hash: hash, safe_block_hash: hash, finalized_block_hash: hash };
        let started = replay_engine.fork_choice_updated_v3(parent_state, Some(attributes)).await?;
        let id = started.payload_id.ok_or_else(|| eyre!("snapshot replay must start a native payload job"))?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let payload = replay_engine.get_payload_v4(id).await?;
        let status = replay_engine.new_payload_v4(payload.execution_payload, payload.parent_beacon_block_root).await?;
        assert_eq!(status.status, alloy_rpc_types_engine::PayloadStatusEnum::Valid, "pre-F snapshot replay must independently validate its next native block");
        let second_hash = status.latest_valid_hash.ok_or_else(|| eyre!("snapshot replay VALID must include exact hash"))?;
        replay_engine.fork_choice_updated_v3(ForkchoiceState { head_block_hash: second_hash, ..parent_state }, None).await?;
        let second = reads.get_block_by_number(BlockNumberOrTag::Latest).await?.unwrap();
        assert_eq!(second.header.number, 2, "actual admitted snapshot replay must advance exactly one block");
        assert_eq!(second.header.parent_hash, hash, "replayed native block must extend exactly the authenticated snapshot tip");
        assert_eq!(second.header.hash, second_hash, "canonical replay hash must equal exact Engine VALID hash");
        drop(replay_engine); drop(reads);
        drop(replay);
        assert!(replay_runtime.graceful_shutdown_with_timeout(Duration::from_secs(10)), "admitted snapshot run must drain normally");
    }
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
