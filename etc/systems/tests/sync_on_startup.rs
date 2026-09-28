//! System test for `--sequencer.sync-on-startup` on an isolated sequencer.
//!
//! An isolated sequencer first runs without the flag on its own execution node and builds a
//! private fork. A new consensus node then restarts on the same execution node with
//! sync-on-startup. It must reorg that execution node onto the canonical chain, seal nothing
//! while syncing, switch to isolated sequencing once, and fork its first private block from a
//! fresh canonical head.

use std::time::{Duration, Instant};

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_genesis::ChainConfig;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, U64, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_engine::JwtSecret;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionRequest;
use base_consensus_node::{NodeMode, SyncOnStartupConfig};
use base_consensus_rpc::AdminApiClient;
use base_system_tests::{
    ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, InProcessBuilder, InProcessBuilderConfig, InProcessConsensus,
    InProcessConsensusConfig, InProcessNodeRuntime, SEQUENCER, SystemTestProviderExt,
    SystemTestStack, SystemTestStackBuilder,
};
use eyre::{Result, WrapErr};
use jsonrpsee::http_client::HttpClientBuilder;
use tokio::time::sleep;

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const L1_SLOT_DURATION: u64 = 1;
const BLOCK_TIMEOUT: Duration = Duration::from_secs(60);
const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);
const TRANSITION_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

#[tokio::test]
async fn isolated_sequencer_syncs_off_private_fork_before_sequencing() -> Result<()> {
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_slot_duration(L1_SLOT_DURATION)
        .build()
        .await?;
    let canonical = system.l2_builder_provider()?;
    canonical.wait_for_block(5, BLOCK_TIMEOUT).await.wrap_err("canonical chain did not start")?;

    let jwt_secret = JwtSecret::random();
    let isolated_el = start_isolated_el(&system, jwt_secret).await?;
    let isolated = RootProvider::<Base>::new_http(isolated_el.rpc_url()?);
    // The active builder serves canonical blocks when reth backfills during the reorg.
    peer_execution_nodes(&isolated, &canonical).await?;

    // Without the flag, an isolated sequencer never ingests canonical blocks, so it builds a
    // private fork.
    let fork_sender = signer(ANVIL_ACCOUNT_5.private_key)?;
    let private_tx_block = {
        let _consensus =
            start_consensus(&system, &isolated_el, jwt_secret, NodeMode::IsolatedSequencer, None)
                .await?;
        let tx = send_transfer(&isolated, &fork_sender, 0).await?;
        let receipt = isolated
            .wait_for_receipt(tx, TX_RECEIPT_TIMEOUT)
            .await
            .wrap_err("isolated sequencer did not seal its private transaction")?;
        let block = receipt.inner.block_number.ok_or_else(|| eyre::eyre!("receipt block"))?;
        let hash = receipt.inner.block_hash.ok_or_else(|| eyre::eyre!("receipt hash"))?;
        assert_ne!(
            canonical.wait_for_block_hash_at(block, BLOCK_TIMEOUT).await?,
            hash,
            "isolated sequencer without sync-on-startup must stay on its private fork"
        );
        (block, hash)
    };
    let canonical_before_sync = canonical.get_block_number().await?;

    let sync_config = SyncOnStartupConfig {
        max_safe_age: Duration::from_secs(120),
        max_unsafe_lag: Duration::from_secs(30),
        timeout: Some(TRANSITION_TIMEOUT),
    };
    let consensus = start_consensus(
        &system,
        &isolated_el,
        jwt_secret,
        NodeMode::IsolatedSequencer,
        Some(sync_config),
    )
    .await?;
    // opp2p works during the sync phase: this is how the node joins the canonical gossip mesh.
    consensus
        .connect_peer(&system.l2_stack().builder_consensus().p2p_addr())
        .await
        .wrap_err("failed to peer the syncing sequencer with the active sequencer")?;

    let admin = HttpClientBuilder::default().build(consensus.rpc_url())?;
    assert!(
        !admin.admin_sequencer_active().await?,
        "sequencer must report inactive during the sync phase"
    );
    let started = Instant::now();
    while !admin.admin_sequencer_active().await? {
        eyre::ensure!(
            started.elapsed() < TRANSITION_TIMEOUT,
            "sync-on-startup did not switch to isolated sequencing"
        );
        sleep(POLL_INTERVAL).await;
    }

    assert_ne!(
        isolated.wait_for_block_hash_at(private_tx_block.0, BLOCK_TIMEOUT).await?,
        private_tx_block.1,
        "sync-on-startup must reorg the execution node off its private fork"
    );

    let post_sender = signer(ANVIL_ACCOUNT_6.private_key)?;
    let tx = send_transfer(&isolated, &post_sender, 0).await?;
    let receipt = isolated
        .wait_for_receipt(tx, TX_RECEIPT_TIMEOUT)
        .await
        .wrap_err("isolated sequencer did not seal after switching")?;
    let private_block = receipt.inner.block_number.ok_or_else(|| eyre::eyre!("receipt block"))?;
    let fork_point = fork_point(&isolated, &canonical, private_block).await?;

    // Every block up to the fork point is canonical, so nothing was sealed locally before the
    // switch, and the first private block extends a canonical head that is fresher than the
    // canonical tip observed before sync started.
    assert!(
        fork_point >= canonical_before_sync,
        "first private block forked from #{fork_point}, behind the canonical tip \
         #{canonical_before_sync} seen before sync started"
    );
    assert_ne!(
        canonical.block_hash_at(fork_point + 1).await?,
        isolated.block_hash_at(fork_point + 1).await?,
        "the block after the fork point must be private"
    );

    drop(consensus);
    isolated_el.shutdown().await?;
    system.shutdown().await
}

/// Adds `canonical` as a trusted devp2p peer of `isolated` and waits for the connection.
async fn peer_execution_nodes(
    isolated: &RootProvider<Base>,
    canonical: &RootProvider<Base>,
) -> Result<()> {
    let info: serde_json::Value = canonical.raw_request("admin_nodeInfo".into(), ()).await?;
    let enode = info["enode"].as_str().ok_or_else(|| eyre::eyre!("admin_nodeInfo has no enode"))?;
    // The reported host may be unroutable; the builder always listens on localhost.
    let (identity, address) =
        enode.split_once('@').ok_or_else(|| eyre::eyre!("malformed enode {enode}"))?;
    let port = address.split('?').next().and_then(|a| a.rsplit(':').next()).unwrap_or_default();
    let enode = format!("{identity}@127.0.0.1:{port}");
    let _: bool = isolated.raw_request("admin_addTrustedPeer".into(), (enode.clone(),)).await?;
    let _: bool = isolated.raw_request("admin_addPeer".into(), (enode,)).await?;

    let started = Instant::now();
    while isolated.raw_request::<_, U64>("net_peerCount".into(), ()).await? == U64::ZERO {
        eyre::ensure!(started.elapsed() < BLOCK_TIMEOUT, "execution nodes did not peer");
        sleep(POLL_INTERVAL).await;
    }
    Ok(())
}

/// Returns the highest block at or below `from` whose hash matches the canonical chain.
async fn fork_point(
    isolated: &RootProvider<Base>,
    canonical: &RootProvider<Base>,
    from: u64,
) -> Result<u64> {
    for number in (0..=from).rev() {
        let local = isolated.block_hash_at(number).await?;
        if local.is_some() && local == canonical.block_hash_at(number).await? {
            return Ok(number);
        }
    }
    eyre::bail!("isolated chain shares no block with the canonical chain")
}

async fn start_isolated_el(
    system: &SystemTestStack,
    jwt_secret: JwtSecret,
) -> Result<InProcessBuilder> {
    let genesis = system.l2_deployment().read_genesis()?;
    let rollup_config = rollup_config(system)?;
    InProcessBuilder::start(InProcessBuilderConfig {
        runtime: InProcessNodeRuntime::SystemTest,
        chain_spec: InProcessBuilderConfig::chain_spec_from_genesis_json(genesis.as_bytes())?,
        datadir: None,
        jwt_secret,
        http_port: None,
        ws_port: None,
        auth_port: None,
        p2p_port: None,
        flashblocks_port: None,
        metrics_port: None,
        enable_experimental_validity_transactions: false,
        payload_builder_cutover: false,
        extra_extensions: Vec::new(),
        block_time: Duration::from_secs(rollup_config.block_time),
        persistence_threshold: None,
        persistence_backpressure_threshold: None,
        txpool_max_transactions: None,
        txpool_max_size_mb: None,
        txpool_max_account_slots: None,
        p2p_secret_key: Some(B256::from_slice(
            PrivateKeySigner::random().credential().to_bytes().as_slice(),
        )),
        // Keep the mempool private so only transactions sent here can make blocks diverge.
        disable_tx_gossip: true,
    })
    .await
    .wrap_err("failed to start isolated execution node")
}

async fn start_consensus(
    system: &SystemTestStack,
    el: &InProcessBuilder,
    jwt_secret: JwtSecret,
    mode: NodeMode,
    sync_on_startup: Option<SyncOnStartupConfig>,
) -> Result<InProcessConsensus> {
    let l1_chain_config: ChainConfig =
        serde_json::from_str(&system.l1_genesis().read_el_genesis()?)
            .wrap_err("failed to parse L1 chain config")?;
    InProcessConsensus::start(InProcessConsensusConfig {
        rollup_config: rollup_config(system)?,
        l1_chain_config,
        jwt_secret,
        l1_rpc_url: system.l1_rpc_url().await?,
        l1_beacon_url: system.l1_beacon_url().await?,
        l2_engine_url: el.engine_url()?,
        mode,
        sequencer_key: None,
        p2p_key: None,
        rpc_port: None,
        p2p_tcp_port: None,
        p2p_udp_port: None,
        unsafe_block_signer: SEQUENCER.address,
        l1_slot_duration_override: Some(L1_SLOT_DURATION),
        sequencer_stopped: false,
        verifier_l1_confs: 0,
        shadow_blocks_per_cycle: None,
        sync_on_startup,
        upgrade_signal: None,
    })
    .await
    .wrap_err("failed to start isolated consensus node")
}

fn rollup_config(system: &SystemTestStack) -> Result<RollupConfig> {
    serde_json::from_str(&system.l2_deployment().read_rollup_config()?)
        .wrap_err("failed to parse rollup config")
}

fn signer(private_key: B256) -> Result<PrivateKeySigner> {
    PrivateKeySigner::from_bytes(&private_key).wrap_err("failed to parse signer")
}

async fn send_transfer(
    provider: &RootProvider<Base>,
    signer: &PrivateKeySigner,
    nonce: u64,
) -> Result<B256> {
    let tx = BaseTransactionRequest::default()
        .from(signer.address())
        .to(Address::repeat_byte(0xde))
        .value(U256::from(1_000_000_000u64))
        .transaction_type(2)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(1_000_000_000)
        .with_max_priority_fee_per_gas(0)
        .with_chain_id(L2_CHAIN_ID)
        .with_nonce(nonce)
        .build_typed_tx()
        .map_err(|e| eyre::eyre!("invalid transaction request: {e:?}"))?;
    let signature = signer.sign_hash_sync(&tx.signature_hash())?;
    let signed = tx.into_signed(signature);
    let raw: Bytes = signed.encoded_2718().into();
    let pending =
        provider.send_raw_transaction(&raw).await.wrap_err("failed to send transaction")?;
    Ok(*pending.tx_hash())
}
