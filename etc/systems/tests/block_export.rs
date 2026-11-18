//! Real-stack coverage for exporting canonical blocks from a node datadir.

use std::{fs, sync::Arc, time::Duration};

use alloy_eips::{BlockNumHash, eip2718::Decodable2718};
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_engine::{ForkchoiceState, JwtSecret};
use base_common_consensus::BaseTxEnvelope;
use base_common_genesis::{ChainGenesis, RollupConfig, SystemConfig, UpgradeConfig};
use base_common_network::BaseEngineApi;
use base_common_rpc_types_engine::BaseExecutionPayload;
use base_consensus_derive::AttributesBuilder;
use base_consensus_engine::EngineEndpoint;
use base_consensus_node::{EngineConfig, NodeOperatingMode, StandaloneAttributesBuilder};
use base_execution_chainspec::BaseChainSpec;
use base_execution_txpool::ValiditySignatureMode;
use base_node_runner::test_utils::{L1_BLOCK_INFO_DEPOSIT_TX, load_chain_spec};
use base_protocol::{BlockInfo, L1BlockInfoTx, L2BlockInfo};
use base_system_tests::{
    BenchmarkCli, BenchmarkCommand, DevnetBlockInterval, EXPORT_BLOCKS_FILE, InProcessBuilder,
    InProcessBuilderConfig, InProcessNodeRuntime, REPLAY_RESULT_FILE, read_export_manifest,
};
use clap::Parser;
use eyre::{OptionExt, Result};
use reth_db::{ClientVersion, init_db, mdbx::DatabaseArguments};
use reth_provider::{BlockNumReader, DatabaseProviderFactory};
use tokio::time::timeout;
use url::Url;

const BLOCKS: u64 = 5;

/// Exporting a builder's blocks from its datadir yields the same `blocks.jsonl` as the snapshot
/// benchmark's export over its RPC, and the export replays onto a node at genesis.
#[tokio::test]
async fn datadir_export_matches_builder_export_and_replays() -> Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();
    let dir = tempfile::tempdir()?;
    // Caller-owned datadirs outlive their node; the builder must stop before a read-only open.
    let empty_datadir = |name: &str| -> Result<std::path::PathBuf> {
        let datadir = dir.path().join(name);
        drop(init_db(datadir.join("db"), DatabaseArguments::new(ClientVersion::default()))?);
        Ok(datadir)
    };
    let datadir = empty_datadir("datadir")?;
    // Jovian is active from genesis, so a validator derives block 1's base fee from the genesis
    // extra data: version 1, denominator 50, elasticity 6, no minimum base fee.
    let mut genesis = load_chain_spec().inner.genesis.clone();
    genesis.extra_data = Bytes::from_static(&[1, 0, 0, 0, 50, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0, 0]);
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));
    let genesis_hash = chain_spec.inner.genesis_hash();
    let genesis_time = chain_spec.inner.genesis.timestamp;
    let jwt_secret = JwtSecret::random();
    let builder = InProcessBuilder::start(InProcessBuilderConfig {
        runtime: InProcessNodeRuntime::Host,
        engine_sharing: Default::default(),
        chain_spec: Arc::clone(&chain_spec),
        datadir: Some(datadir.clone()),
        jwt_secret,
        http_port: None,
        ws_port: None,
        auth_port: None,
        p2p_port: None,
        metrics_port: None,
        validity_signature_mode: ValiditySignatureMode::Off,
        extra_extensions: Vec::new(),
        persistence_threshold: Some(0),
        persistence_backpressure_threshold: None,
        txpool_max_transactions: None,
        txpool_max_size_mb: None,
        txpool_max_account_slots: None,
    })
    .await?;
    let rpc_url = builder.rpc_url()?;

    let l1_info_deposit = BaseTxEnvelope::decode_2718(&mut L1_BLOCK_INFO_DEPOSIT_TX.as_ref())?;
    let l1_info = L1BlockInfoTx::decode_calldata(
        &l1_info_deposit.as_deposit().ok_or_eyre("L1-info fixture is not a deposit")?.input,
    )?;
    let system_config = SystemConfig {
        gas_limit: chain_spec.inner.genesis.gas_limit,
        eip1559_denominator: Some(50),
        eip1559_elasticity: Some(6),
        ..Default::default()
    };
    let rollup = Arc::new(RollupConfig {
        l2_chain_id: chain_spec.inner.chain.id().into(),
        block_time: 2,
        genesis: ChainGenesis {
            l1: l1_info.id(),
            l2: BlockNumHash { number: 0, hash: genesis_hash },
            l2_time: genesis_time,
            system_config: Some(system_config),
        },
        upgrades: UpgradeConfig {
            regolith_time: Some(0),
            canyon_time: Some(0),
            delta_time: Some(0),
            ecotone_time: Some(0),
            fjord_time: Some(0),
            granite_time: Some(0),
            holocene_time: Some(0),
            isthmus_time: Some(0),
            jovian_time: Some(0),
            ..Default::default()
        },
        ..Default::default()
    });
    let engine = EngineConfig {
        config: Arc::clone(&rollup),
        l2_endpoint: EngineEndpoint::Url(builder.engine_url()?),
        l2_jwt_secret: jwt_secret,
        l1_url: Url::parse("http://127.0.0.1:1")?,
        mode: NodeOperatingMode::Sequencer,
        l1_rpc_timeout: base_consensus_providers::L1_RPC_TIMEOUT,
    }
    .build_engine_client()
    .await?;
    let fork_choice = |head| ForkchoiceState {
        head_block_hash: head,
        safe_block_hash: genesis_hash,
        finalized_block_hash: genesis_hash,
    };
    let mut parent = L2BlockInfo::new(
        BlockInfo::new(genesis_hash, 0, B256::ZERO, genesis_time),
        l1_info.id(),
        l1_info.sequence_number(),
    );
    let mut attributes_builder =
        StandaloneAttributesBuilder::new(Arc::clone(&rollup), l1_info, system_config, None);
    for _ in 1..=BLOCKS {
        let attributes =
            attributes_builder.prepare_payload_attributes(parent, l1_info.id()).await?;
        let payload_id = engine
            .fork_choice_updated_v3(fork_choice(parent.block_info.hash), Some(attributes))
            .await?
            .payload_id
            .ok_or_eyre("build did not start")?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let payload = engine.get_payload_v4(payload_id).await?.execution_payload;
        let status = engine.new_payload_v4(payload.clone(), B256::ZERO).await?;
        let hash = status.latest_valid_hash.ok_or_eyre("payload rejected")?;
        engine.fork_choice_updated_v3(fork_choice(hash), None).await?;
        let payload = BaseExecutionPayload::V4(payload);
        parent = L2BlockInfo::from_payload_and_genesis(payload, Some(B256::ZERO), &rollup.genesis)?;
    }
    timeout(Duration::from_secs(20), async {
        while builder.provider.database_provider_ro()?.best_block_number()? < BLOCKS {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        eyre::Ok(())
    })
    .await??;

    let chain = dir.path().join("genesis.json");
    fs::write(&chain, serde_json::to_vec(&chain_spec.inner.genesis)?)?;
    let parse =
        |args: String| BenchmarkCli::try_parse_from(args.split_whitespace()).map(|cli| cli.command);
    let builder_export = dir.path().join("builder-export");
    let Some(BenchmarkCommand::Snapshot(snapshot)) = parse(format!(
        "base-bench snapshot --builder-datadir {0} --load-test-config {0} --output-dir {0} --scenario export",
        dir.path().display()
    ))?
    else {
        unreachable!("parsed a snapshot command")
    };
    snapshot
        .export_produced_blocks(
            &builder_export,
            &rpc_url,
            0,
            genesis_hash,
            DevnetBlockInterval::TwoSeconds,
        )
        .await?;
    builder.shutdown().await?;
    let datadir_export = dir.path().join("datadir-export");
    let Some(BenchmarkCommand::Export(export)) = parse(format!(
        "base-bench export --chain {} --datadir {} --from 1 --output-dir {}",
        chain.display(),
        datadir.display(),
        datadir_export.display()
    ))?
    else {
        unreachable!("parsed an export command")
    };
    export.run().await?;

    assert_eq!(
        fs::read(datadir_export.join(EXPORT_BLOCKS_FILE))?,
        fs::read(builder_export.join(EXPORT_BLOCKS_FILE))?
    );
    let manifest = read_export_manifest(&datadir_export)?;
    assert_eq!(manifest.source.map(|source| source.head_number), Some(BLOCKS));
    assert_eq!((manifest.boundary_number, manifest.boundary_hash), (0, genesis_hash));

    let replay_datadir = empty_datadir("replay-datadir")?;
    let replay_output = dir.path().join("replay");
    let Some(BenchmarkCommand::Replay(replay)) = parse(format!(
        "base-bench replay --datadir {} --export-dir {} --output-dir {} --shutdown-timeout-seconds 30",
        replay_datadir.display(),
        datadir_export.display(),
        replay_output.display()
    ))?
    else {
        unreachable!("parsed a replay command")
    };
    replay.run().await?;
    let result: serde_json::Value =
        serde_json::from_slice(&fs::read(replay_output.join(REPLAY_RESULT_FILE))?)?;
    assert_eq!(result["match"], true);
    Ok(())
}
