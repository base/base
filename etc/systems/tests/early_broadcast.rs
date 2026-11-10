//! Real-stack coverage for the searcher timing problem solved by early block broadcast.
//!
//! Since the Denim network upgrade (200 ms blocks), a block that froze early still waited for
//! the 150 ms seal deadline before it was gossiped. A searcher reacting to block B then reached
//! the sequencer after block B+1's build had snapshotted the pool, and landed in block B+2.
//! Sealing on the execution layer's frozen-payload hint gossips block B early enough for the
//! same searcher to land in block B+1.
//!
//! The execution node is launched as `base sequencer` launches it, and drives the production
//! engine and sequencer actors over the in-process Engine API at wall-clock cadence. L1
//! derivation, conductor, and P2P are not started: the test reads the payloads the sequencer
//! hands to gossip, and models the network between sequencer and searcher as a fixed delay.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_consensus::SignableTransaction;
use alloy_eips::{
    BlockNumHash,
    eip2718::{Decodable2718, Encodable2718},
};
use alloy_network::{ReceiptResponse, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_engine::{ForkchoiceState, JwtSecret};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_builder_core::{BuilderConfig, NativePayloadServiceBuilder};
use base_common_consensus::BaseTxEnvelope;
use base_common_genesis::{BaseUpgrade, ChainGenesis, RollupConfig, SystemConfig, UpgradeConfig};
use base_common_network::{Base, BaseEngineApi};
use base_common_rpc_types::BaseTransactionRequest;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_derive::AttributesBuilder;
use base_consensus_engine::{Engine, EngineEndpoint, EngineState, InMemoryTransport};
use base_consensus_node::{
    ConductorClient, EngineActor, EngineConfig, EngineProcessor, NodeActor, NodeOperatingMode,
    PayloadBuilder, QueuedSequencerEngineClient, QueuedUnsafePayloadGossipClient,
    RecoveryModeGuard, SequencerActor, SequencerEngineRequestCoordinator,
    StandaloneAttributesBuilder, StandaloneDerivationClient, StandaloneOriginSelector,
};
use base_execution_chainspec::BaseChainSpec;
use base_node_core::args::RollupArgs;
use base_node_runner::{
    BaseNodeRunner, LaunchedBaseNode,
    test_utils::{L1_BLOCK_INFO_DEPOSIT_TX, load_chain_spec},
};
use base_protocol::{BlockInfo, DEFAULT_SEAL_OFFSET, L1BlockInfoTx, L2BlockInfo};
use base_system_tests::{ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_2};
use eyre::{OptionExt, Result, bail, eyre};
use reth_db::{ClientVersion, init_db, mdbx::DatabaseArguments};
use reth_ethereum_forks::ForkCondition;
use reth_node_builder::{NodeBuilder, NodeConfig};
use reth_node_core::args::{DatadirArgs, NetworkArgs, RpcServerArgs};
use reth_tasks::Runtime;
use tokio::{
    sync::{mpsc, watch},
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use tracing::info;
use url::Url;

const LEGACY_BLOCK_TIME: u64 = 2;
const TEST_TIMEOUT: Duration = Duration::from_secs(60);
const STEP_TIMEOUT: Duration = Duration::from_secs(20);
/// Delay from the sequencer gossiping a block to the searcher's reaction reaching the sequencer:
/// P2P hops to the searcher's node, then its transaction through proxyd and the mempool nodes.
/// The early broadcast design puts this at 50-85 ms on mainnet; this is the slow end.
const SEARCHER_ROUND_TRIP: Duration = Duration::from_millis(85);
/// Interval between Denim blocks: the build of block B+1 starts this long after block B's.
const BLOCK_INTERVAL: Duration =
    Duration::from_millis(RollupConfig::NATIVE_SUBSECOND_BLOCK_INTERVAL_MILLIS);
/// Opportunities the searcher reacts to in each scenario.
const REACTIONS: u64 = 3;

/// One searcher reaction to a block carrying an opportunity.
#[derive(Debug)]
struct Reaction {
    /// Block B, which included the opportunity.
    opportunity_block: u64,
    /// Block that included the searcher's reaction.
    landed_block: u64,
    /// When block B was gossiped, measured from the scheduled start of its build.
    gossiped_after: Duration,
    /// When the reaction reached the sequencer, measured from the same instant.
    reached_sequencer_after: Duration,
}

/// A searcher that reacts to opportunities it sees in gossiped blocks.
struct Searcher {
    rpc: RootProvider<Base>,
    gossip: mpsc::Receiver<BaseExecutionPayloadEnvelope>,
    rollup_config: Arc<RollupConfig>,
    chain_id: u64,
}

impl Searcher {
    async fn next_gossiped_block(&mut self) -> Result<BaseExecutionPayloadEnvelope> {
        timeout(STEP_TIMEOUT, self.gossip.recv()).await?.ok_or_eyre("sequencer stopped gossiping")
    }

    /// Wall-clock time at which the sequencer is scheduled to start building `block_number`.
    fn build_start(&self, block_number: u64) -> SystemTime {
        UNIX_EPOCH
            + Duration::from_millis(self.rollup_config.l2_block_timestamp_millis(block_number - 1))
    }

    /// Waits until blocks are built in their scheduled slots rather than catching up at startup:
    /// only a block built in its own slot is gossiped before its timestamp.
    async fn await_scheduled_cadence(&mut self) -> Result<()> {
        loop {
            let number = self.next_gossiped_block().await?.execution_payload.block_number();
            if SystemTime::now() < self.build_start(number + 1) {
                return Ok(());
            }
        }
    }

    /// Submits an opportunity, reacts to the block that includes it, and reports where the
    /// reaction landed.
    async fn react(&mut self, nonce: u64) -> Result<Reaction> {
        let opportunity = transfer(&ANVIL_ACCOUNT_1.private_key, nonce, self.chain_id)?;
        let _pending = self.rpc.send_raw_transaction(&opportunity).await?;
        let opportunity_block = loop {
            let payload = self.next_gossiped_block().await?.execution_payload;
            if payload.transactions().contains(&opportunity) {
                break payload.block_number();
            }
        };
        let build_start = self.build_start(opportunity_block);
        let gossiped_after = SystemTime::now().duration_since(build_start)?;

        tokio::time::sleep(SEARCHER_ROUND_TRIP).await;
        let reaction = transfer(&ANVIL_ACCOUNT_2.private_key, nonce, self.chain_id)?;
        let hash = *self.rpc.send_raw_transaction(&reaction).await?.tx_hash();
        let reached_sequencer_after = SystemTime::now().duration_since(build_start)?;

        let landed_block = timeout(STEP_TIMEOUT, async {
            loop {
                if let Some(receipt) = self.rpc.get_transaction_receipt(hash).await? {
                    eyre::ensure!(receipt.status(), "searcher reaction reverted");
                    return receipt.block_number().ok_or_eyre("receipt missing block number");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        Ok(Reaction { opportunity_block, landed_block, gossiped_after, reached_sequencer_after })
    }
}

fn transfer(private_key: &B256, nonce: u64, chain_id: u64) -> Result<Bytes> {
    let signer = PrivateKeySigner::from_bytes(private_key)?;
    let transaction = BaseTransactionRequest::default()
        .from(signer.address())
        .to(Address::repeat_byte(0xfe))
        .value(U256::from(1))
        .transaction_type(2)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(2_000_000_000)
        .with_max_priority_fee_per_gas(1_000_000)
        .with_chain_id(chain_id)
        .with_nonce(nonce)
        .build_typed_tx()
        .map_err(|error| eyre!("invalid transaction: {error:?}"))?;
    let signature = signer.sign_hash_sync(&transaction.signature_hash())?;
    Ok(transaction.into_signed(signature).encoded_2718().into())
}

/// Runs a sequencer that seals on frozen-payload hints when `early_broadcast` is set, and on the
/// seal deadline alone otherwise, and returns a searcher's reactions to its blocks.
async fn searcher_reactions(early_broadcast: bool) -> Result<Vec<Reaction>> {
    base_node_runner::test_utils::init_silenced_tracing();
    // Denim is active from block 1, which is scheduled two seconds from now so the chain follows
    // wall-clock time as it does in production.
    let genesis_time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let activation = genesis_time + LEGACY_BLOCK_TIME;
    let mut genesis = load_chain_spec().inner.genesis.clone();
    genesis.timestamp = genesis_time;
    let mut chain_spec = BaseChainSpec::from_genesis(genesis);
    chain_spec.set_fork(BaseUpgrade::Denim, ForkCondition::Timestamp(activation));
    let chain_spec = Arc::new(chain_spec);
    let chain_id = chain_spec.inner.chain.id();

    let datadir = tempfile::tempdir()?;
    let mut network = NetworkArgs::default().with_unused_ports();
    network.discovery.disable_discovery = true;
    let mut rpc = RpcServerArgs::default().with_unused_ports().with_http();
    rpc.http_api = Some("eth".parse()?);
    let node_config = NodeConfig::new(Arc::clone(&chain_spec))
        .with_network(network)
        .with_rpc(rpc)
        .with_unused_ports()
        .with_datadir_args(DatadirArgs {
            datadir: datadir.path().to_string_lossy().parse()?,
            ..Default::default()
        });
    let db = init_db(
        node_config.datadir().db(),
        DatabaseArguments::new(ClientVersion::default())
            .with_geometry_max_size(Some(100 * 1024 * 1024)),
    )?;
    let runtime = Runtime::test();
    // The same payload service and runner handoff as `base sequencer`, so frozen-payload hints
    // reach the consensus layer through the production wiring.
    let LaunchedBaseNode { handle, auth_module, engine_api, payload_ready } =
        BaseNodeRunner::new(RollupArgs::default())
            .with_service_builder(NativePayloadServiceBuilder::new(BuilderConfig::default()))
            .launch(
                NodeBuilder::new(node_config)
                    .with_database(db)
                    .with_launch_context(runtime.clone()),
            )
            .await?;
    let http = handle.node.rpc_server_handle().http_local_addr().ok_or_eyre("HTTP RPC unbound")?;
    let rpc = RootProvider::new(RpcClient::builder().http(format!("http://{http}").parse()?));

    let l1_info_deposit = BaseTxEnvelope::decode_2718(&mut L1_BLOCK_INFO_DEPOSIT_TX.as_ref())?;
    let l1_info = L1BlockInfoTx::decode_calldata(
        &l1_info_deposit.as_deposit().ok_or_eyre("L1-info fixture is not a deposit")?.input,
    )?;
    let genesis_hash = chain_spec.inner.genesis_hash();
    let genesis_system_config = SystemConfig {
        gas_limit: chain_spec.inner.genesis.gas_limit,
        eip1559_denominator: Some(50),
        eip1559_elasticity: Some(6),
        ..Default::default()
    };
    let mut rollup_config = RollupConfig {
        l2_chain_id: chain_id.into(),
        block_time: LEGACY_BLOCK_TIME,
        genesis: ChainGenesis {
            l1: l1_info.id(),
            l2: BlockNumHash { number: 0, hash: genesis_hash },
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
    rollup_config.set_upgrade_activation_timestamp(BaseUpgrade::Denim, activation);
    let rollup_config = Arc::new(rollup_config);
    let system_config = SystemConfig {
        gas_limit: genesis_system_config.gas_limit
            / u64::from(RollupConfig::DENIM_GAS_PARAMETER_SCALING_FACTOR),
        ..genesis_system_config
    };

    let engine = Arc::new(
        EngineConfig {
            config: Arc::clone(&rollup_config),
            l2_endpoint: EngineEndpoint::InMemory {
                transport: InMemoryTransport { module: auth_module },
                engine_api,
            },
            l2_jwt_secret: JwtSecret::random(),
            // The engine client constructs this provider lazily. L1-free sequencing never issues
            // an L1 request.
            l1_url: Url::parse("http://127.0.0.1:1")?,
            mode: NodeOperatingMode::Sequencer,
            l1_rpc_timeout: base_consensus_providers::L1_RPC_TIMEOUT,
        }
        .build_engine_client()
        .await?,
    );

    // L1-free sequencing needs a non-genesis head, so build the first Denim block directly.
    let attributes_builder =
        StandaloneAttributesBuilder::new(Arc::clone(&rollup_config), l1_info, system_config, None);
    let genesis_head = L2BlockInfo::new(
        BlockInfo::new(genesis_hash, 0, B256::ZERO, genesis_time),
        l1_info.id(),
        l1_info.sequence_number(),
    );
    let attributes =
        attributes_builder.clone().prepare_payload_attributes(genesis_head, l1_info.id()).await?;
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
    let payload = engine.get_payload_v4(payload_id).await?.execution_payload;
    let status = engine.new_payload_v4(payload, B256::ZERO).await?;
    let boundary_hash = status.latest_valid_hash.ok_or_eyre("boundary payload rejected")?;
    engine.fork_choice_updated_v3(fork_choice(boundary_hash), None).await?;

    // The production engine and sequencer actors, without derivation, conductor, or P2P.
    let cancellation = CancellationToken::new();
    let (engine_actor_request_tx, engine_actor_request_rx) = mpsc::channel(1024);
    let (unsafe_head_tx, unsafe_head_rx) = watch::channel(L2BlockInfo::default());
    let (engine_state_tx, engine_state_rx) = watch::channel(EngineState::default());
    let (engine_queue_length_tx, _engine_queue_length_rx) = watch::channel(0);
    // Resets would walk L1 origins; the bootstrapped execution head is retained instead.
    let processor = EngineProcessor::new_skip_reset(
        Arc::clone(&engine),
        Arc::clone(&rollup_config),
        StandaloneDerivationClient,
        Engine::new(EngineState::default(), engine_state_tx, engine_queue_length_tx),
    );
    let engine_actor = EngineActor::new(
        cancellation.clone(),
        engine_actor_request_rx,
        SequencerEngineRequestCoordinator::new(
            processor,
            NodeOperatingMode::Sequencer,
            None,
            false,
            unsafe_head_tx,
        ),
    );
    let sequencer_engine_client = Arc::new(QueuedSequencerEngineClient {
        engine_actor_request_tx,
        unsafe_head_rx,
        engine_state_rx,
    });
    let (_admin_api_tx, admin_api_rx) = mpsc::channel(1);
    // The queue the network actor drains to gossip sealed payloads; the searcher reads it instead.
    let (gossip_tx, gossip) = mpsc::channel(1024);
    let recovery_mode = RecoveryModeGuard::new(false);
    let sequencer_actor: SequencerActor<_, ConductorClient, _, _, _> = SequencerActor {
        admin_api_rx,
        builder: PayloadBuilder {
            attributes_builder,
            engine_client: Arc::clone(&sequencer_engine_client),
            origin_selector: StandaloneOriginSelector::new(l1_info),
            recovery_mode: recovery_mode.clone(),
            rollup_config: Arc::clone(&rollup_config),
        },
        cancellation_token: cancellation.clone(),
        conductor: None,
        engine_client: sequencer_engine_client,
        is_active: true,
        mode: NodeOperatingMode::Sequencer,
        shadow_funding: None,
        recovery_mode,
        rollup_config: Arc::clone(&rollup_config),
        seal_offset: DEFAULT_SEAL_OFFSET,
        schedule_delay: Duration::ZERO,
        unsafe_payload_gossip_client: QueuedUnsafePayloadGossipClient::new(gossip_tx),
        sealer: None,
        pending_stop: None,
        // Without hints the sequencer seals on its deadline, as it did before early broadcast.
        payload_ready: early_broadcast.then(|| payload_ready.resubscribe()),
    };
    let mut engine_actor = tokio::spawn(engine_actor.start(()));
    let mut sequencer_actor = tokio::spawn(sequencer_actor.start(()));

    let mut searcher = Searcher { rpc, gossip, rollup_config, chain_id };
    let reactions = tokio::select! {
        exit = &mut engine_actor => bail!("engine actor stopped: {exit:?}"),
        exit = &mut sequencer_actor => bail!("sequencer actor stopped: {exit:?}"),
        reactions = async {
            searcher.await_scheduled_cadence().await?;
            let mut reactions = Vec::new();
            for nonce in 0..REACTIONS {
                let reaction = searcher.react(nonce).await?;
                info!(early_broadcast, reaction = ?reaction, "searcher reacted to gossiped block");
                reactions.push(reaction);
            }
            Ok::<_, eyre::Report>(reactions)
        } => reactions?,
    };

    cancellation.cancel();
    let _ = tokio::join!(engine_actor, sequencer_actor);
    timeout(STEP_TIMEOUT, runtime.initiate_graceful_shutdown()?.ignore_guard()).await?;
    handle.node_exit_future.await?;
    Ok(reactions)
}

/// A block gossiped as soon as it freezes leaves the searcher time to land in the next block.
#[tokio::test(flavor = "multi_thread")]
async fn searcher_reacting_to_early_broadcast_lands_in_next_block() -> Result<()> {
    let reactions = timeout(TEST_TIMEOUT, searcher_reactions(true)).await??;
    for reaction in reactions {
        assert!(
            reaction.gossiped_after < DEFAULT_SEAL_OFFSET,
            "frozen block waited for the seal deadline: {reaction:?}"
        );
        assert!(
            reaction.reached_sequencer_after < BLOCK_INTERVAL,
            "reaction reached the sequencer after the next build started: {reaction:?}"
        );
        assert_eq!(
            reaction.landed_block,
            reaction.opportunity_block + 1,
            "searcher missed the next block: {reaction:?}"
        );
    }
    Ok(())
}

/// A block held until the seal deadline reaches the searcher too late for the next block, which
/// was every block before early broadcast and remains the fallback when no hint arrives.
#[tokio::test(flavor = "multi_thread")]
async fn searcher_reacting_to_deadline_sealed_block_lands_one_block_later() -> Result<()> {
    let reactions = timeout(TEST_TIMEOUT, searcher_reactions(false)).await??;
    for reaction in reactions {
        assert!(
            reaction.gossiped_after >= DEFAULT_SEAL_OFFSET,
            "block was gossiped before the seal deadline without a hint: {reaction:?}"
        );
        assert_eq!(
            reaction.landed_block,
            reaction.opportunity_block + 2,
            "searcher did not land two blocks after the opportunity: {reaction:?}"
        );
    }
    Ok(())
}
