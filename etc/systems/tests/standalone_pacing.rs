//! Real-stack coverage for extending historical state with the standalone sequencer.

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy_eips::{BlockNumHash, BlockNumberOrTag, eip2718::Decodable2718};
use alloy_primitives::B256;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_engine::{ForkchoiceState, JwtSecret};
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_genesis::{BaseUpgrade, ChainGenesis, RollupConfig, SystemConfig, UpgradeConfig};
use base_common_network::{Base, BaseEngineApi};
use base_consensus_derive::AttributesBuilder;
use base_consensus_engine::BaseEngineClient;
use base_consensus_node::{
    EngineConfig, NodeOperatingMode, StandaloneAttributesBuilder, StandaloneDenimSchedule,
    StandaloneSequencerNode,
};
use base_execution_chainspec::BaseChainSpec;
use base_execution_txpool::ValiditySignatureMode;
use base_node_runner::test_utils::{L1_BLOCK_INFO_DEPOSIT_TX, load_chain_spec};
use base_protocol::{BaseTimeUpdateTx, BlockInfo, L1BlockInfoTx, L2BlockInfo, L2BlockMetadata};
use base_system_tests::{InProcessBuilder, InProcessBuilderConfig, InProcessNodeRuntime};
use eyre::{OptionExt, Result};
use reth_ethereum_forks::ForkCondition;
use tokio::{task::JoinHandle, time::timeout};
use tokio_util::sync::CancellationToken;
use url::Url;

const SNAPSHOT_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);
const LEGACY_BLOCK_TIME: u64 = 2;
const DENIM_BLOCKS_PER_SECOND: u64 = 5;

/// Extends a three-day-old pre-Denim head through Denim activation and a restart.
#[tokio::test]
async fn standalone_sequencer_paces_historical_denim_activation() -> Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();
    let genesis_time = (SystemTime::now() - SNAPSHOT_AGE).duration_since(UNIX_EPOCH)?.as_secs();
    // Block 1 is the pre-Denim boundary, so Denim activates at block 2's legacy slot.
    let activation = genesis_time + 2 * LEGACY_BLOCK_TIME;
    let mut genesis = load_chain_spec().inner.genesis.clone();
    genesis.timestamp = genesis_time;
    let mut chain_spec = BaseChainSpec::from_genesis(genesis);
    chain_spec.set_fork(BaseUpgrade::Denim, ForkCondition::Timestamp(activation));
    let chain_spec = Arc::new(chain_spec);
    let jwt_secret = JwtSecret::random();
    let builder = InProcessBuilder::start(InProcessBuilderConfig {
        runtime: InProcessNodeRuntime::Host,
        chain_spec: Arc::clone(&chain_spec),
        datadir: None,
        jwt_secret,
        http_port: None,
        ws_port: None,
        auth_port: None,
        p2p_port: None,
        flashblocks_port: None,
        metrics_port: None,
        payload_builder_cutover: true,
        validity_signature_mode: ValiditySignatureMode::Off,
        extra_extensions: Vec::new(),
        block_time: Duration::from_millis(200),
        persistence_threshold: None,
        persistence_backpressure_threshold: None,
        txpool_max_transactions: None,
        txpool_max_size_mb: None,
        txpool_max_account_slots: None,
    })
    .await?;
    let provider = RootProvider::<Base>::new_http(builder.rpc_url()?);
    let engine_url = builder.engine_url()?;

    let l1_info_deposit = BaseTxEnvelope::decode_2718(&mut L1_BLOCK_INFO_DEPOSIT_TX.as_ref())?;
    let l1_info = L1BlockInfoTx::decode_calldata(
        &l1_info_deposit.as_deposit().ok_or_eyre("L1-info fixture is not a deposit")?.input,
    )?;
    let genesis_system_config = SystemConfig {
        gas_limit: chain_spec.inner.genesis.gas_limit,
        eip1559_denominator: Some(50),
        eip1559_elasticity: Some(6),
        ..Default::default()
    };
    let canonical = RollupConfig {
        l2_chain_id: chain_spec.inner.chain.id().into(),
        block_time: LEGACY_BLOCK_TIME,
        genesis: ChainGenesis {
            l1: l1_info.id(),
            l2: BlockNumHash { number: 0, hash: chain_spec.inner.genesis_hash() },
            l2_time: genesis_time,
            system_config: Some(genesis_system_config),
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
    };

    // Standalone sequencing needs a non-genesis head, so build the legacy boundary directly.
    let genesis_hash = canonical.genesis.l2.hash;
    let genesis_head = L2BlockInfo::new(
        BlockInfo::new(genesis_hash, 0, B256::ZERO, genesis_time),
        l1_info.id(),
        l1_info.sequence_number(),
    );
    let attributes = StandaloneAttributesBuilder::new(
        Arc::new(canonical.clone()),
        l1_info,
        genesis_system_config,
        None,
    )
    .prepare_payload_attributes(genesis_head, l1_info.id())
    .await?;
    let engine = engine_client(&engine_url, jwt_secret, Arc::new(canonical.clone())).await?;
    let fork_choice = |head| ForkchoiceState {
        head_block_hash: head,
        safe_block_hash: genesis_hash,
        finalized_block_hash: genesis_hash,
    };
    let payload_id = engine
        .fork_choice_updated_v3(fork_choice(genesis_hash), Some(attributes))
        .await?
        .payload_id
        .ok_or_eyre("boundary build did not start")?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let payload = engine.get_payload_v4(payload_id).await?.execution_payload;
    let status = engine.new_payload_v4(payload, B256::ZERO).await?;
    let boundary_hash = status.latest_valid_hash.ok_or_eyre("boundary payload rejected")?;
    engine.fork_choice_updated_v3(fork_choice(boundary_hash), None).await?;

    // Negative control: the EL rejects a legacy block in the Denim slot, which stops a sequencer
    // without the matching schedule at the boundary.
    let legacy = Sequencer::start(
        &engine_url,
        jwt_secret,
        canonical.clone(),
        l1_info,
        genesis_system_config,
        false,
    )
    .await?;
    let _ = timeout(Duration::from_secs(20), legacy.handle).await?;
    assert_eq!(provider.get_block_number().await?, 1, "EL accepted a mismatched Denim schedule");

    let boundary = L2BlockMetadata::from_block(&block(&provider, 1).await?, &canonical)?;
    let schedule = StandaloneDenimSchedule::new(canonical, &boundary)?;
    assert_eq!(
        schedule.rollup_config.denim_timestamp_schedule(),
        chain_spec.denim_timestamp_schedule()?
    );
    assert_eq!(schedule.system_config.gas_limit, boundary.system_config.gas_limit / 10);

    let started = Instant::now();
    let paced = Sequencer::start(
        &engine_url,
        jwt_secret,
        schedule.rollup_config.clone(),
        l1_info,
        schedule.system_config,
        true,
    )
    .await?;
    let last = 2 + DENIM_BLOCKS_PER_SECOND + 1;
    wait_for_block(&provider, last).await?;
    // One legacy interval to the first Denim block, then six 200ms slots, less the seal lead.
    assert!(started.elapsed() >= Duration::from_millis(3_000), "blocks were not paced");
    paced.stop().await;
    for number in 2..=last {
        let block = block(&provider, number).await?;
        let offset = number - 2;
        let millis = BaseTimeUpdateTx::extract_from_transactions(&block.body.transactions, number)?
            .timestamp_millis_part();
        assert_eq!(
            (block.header.timestamp, u64::from(millis)),
            (activation + offset / DENIM_BLOCKS_PER_SECOND, offset % DENIM_BLOCKS_PER_SECOND * 200)
        );
        let metadata = L2BlockMetadata::from_block(&block, &schedule.rollup_config)?;
        assert_eq!(metadata.system_config.gas_limit, schedule.system_config.gas_limit);
        assert_eq!(
            metadata.system_config.eip1559_denominator,
            boundary.system_config.eip1559_denominator.map(|denominator| denominator * 10)
        );
    }

    // A restart resumes from the head without catching up on the downtime or rescaling.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let head_number = provider.get_block_number().await?;
    let head_block = block(&provider, head_number).await?;
    let head = L2BlockMetadata::from_block(&head_block, &schedule.rollup_config)?;
    let restarted = StandaloneDenimSchedule::new(schedule.rollup_config.clone(), &head)?;
    assert_eq!(restarted.rollup_config, schedule.rollup_config);
    assert_eq!(restarted.system_config, head.system_config);
    let started = Instant::now();
    let resumed = Sequencer::start(
        &engine_url,
        jwt_secret,
        restarted.rollup_config.clone(),
        head.l1_info,
        restarted.system_config,
        true,
    )
    .await?;
    wait_for_block(&provider, head_number + 5).await?;
    assert!(started.elapsed() >= Duration::from_millis(800), "restart caught up on downtime");
    resumed.stop().await;
    let next = block(&provider, head_number + 1).await?;
    assert_eq!(
        BaseTimeUpdateTx::extract_timestamp_ms(
            &next.body.transactions,
            head_number + 1,
            next.header.timestamp
        )?,
        BaseTimeUpdateTx::extract_timestamp_ms(
            &head_block.body.transactions,
            head_number,
            head_block.header.timestamp
        )? + 200
    );

    builder.shutdown().await
}

struct Sequencer {
    cancellation: CancellationToken,
    handle: JoinHandle<Result<(), String>>,
}

impl Sequencer {
    async fn start(
        engine_url: &Url,
        jwt_secret: JwtSecret,
        rollup_config: RollupConfig,
        l1_info: L1BlockInfoTx,
        system_config: SystemConfig,
        pace_from_head: bool,
    ) -> Result<Self> {
        let rollup_config = Arc::new(rollup_config);
        let engine_client =
            engine_client(engine_url, jwt_secret, Arc::clone(&rollup_config)).await?;
        let mut node = StandaloneSequencerNode::new(
            rollup_config,
            Arc::new(engine_client),
            l1_info,
            system_config,
            None,
        );
        node.pace_from_head = pace_from_head;
        let cancellation = CancellationToken::new();
        let token = cancellation.clone();
        let handle = tokio::spawn(async move { node.start_with_cancellation(token).await });
        Ok(Self { cancellation, handle })
    }

    async fn stop(self) {
        self.cancellation.cancel();
        let _ = self.handle.await;
    }
}

async fn engine_client(
    engine_url: &Url,
    jwt_secret: JwtSecret,
    rollup_config: Arc<RollupConfig>,
) -> Result<BaseEngineClient<RootProvider, RootProvider<Base>>> {
    Ok(EngineConfig {
        config: rollup_config,
        l2_url: engine_url.clone(),
        l2_jwt_secret: jwt_secret,
        // Standalone sequencing never issues an L1 request.
        l1_url: Url::parse("http://127.0.0.1:1")?,
        mode: NodeOperatingMode::Sequencer,
        l1_rpc_timeout: base_consensus_providers::L1_RPC_TIMEOUT,
    }
    .build_engine_client()
    .await?)
}

async fn block(provider: &RootProvider<Base>, number: u64) -> Result<BaseBlock> {
    Ok(provider
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .full()
        .await?
        .ok_or_eyre("block is missing")?
        .map_header(|header| header.into_inner())
        .into_consensus()
        .map_transactions(|transaction| transaction.inner.inner.into_inner()))
}

async fn wait_for_block(provider: &RootProvider<Base>, number: u64) -> Result<()> {
    timeout(Duration::from_secs(20), async {
        while provider.get_block_number().await? < number {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    })
    .await?
}
