//! Real-stack coverage for persisting and restoring a standalone development chain.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_consensus::SignableTransaction;
use alloy_eips::{
    BlockNumHash, BlockNumberOrTag,
    eip2718::{Decodable2718, Encodable2718},
};
use alloy_network::{ReceiptResponse, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_engine::{ForkchoiceState, JwtSecret};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_genesis::{
    ChainGenesis, RollupConfig, RuntimeUpgradeRegistry, SystemConfig, UpgradeConfig,
};
use base_common_network::{Base, BaseEngineApi};
use base_common_rpc_types::BaseTransactionRequest;
use base_consensus_derive::AttributesBuilder;
use base_consensus_node::{
    EngineConfig, NodeOperatingMode, StandaloneAttributesBuilder, StandaloneDevChain,
    StandaloneDevError, StandaloneDevState,
};
use base_execution_chainspec::BaseChainSpec;
use base_execution_txpool::ValiditySignatureMode;
use base_node_runner::test_utils::{L1_BLOCK_INFO_DEPOSIT_TX, load_chain_spec};
use base_protocol::{BaseTimeUpdateTx, BlockInfo, L1BlockInfoTx, L2BlockInfo};
use base_system_tests::{
    ANVIL_ACCOUNT_1, InProcessBuilder, InProcessBuilderConfig, InProcessNodeRuntime,
};
use eyre::{OptionExt, Result};
use tokio::{task::JoinHandle, time::timeout};
use tokio_util::sync::CancellationToken;
use url::Url;

const SNAPSHOT_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);
const LEGACY_BLOCK_TIME: u64 = 2;

/// Persists the schedule chosen for a pre-Denim head and restores it after restarts.
#[tokio::test]
async fn standalone_dev_chain_persists_and_restores_schedule() -> Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();
    let genesis_time = (SystemTime::now() - SNAPSHOT_AGE).duration_since(UNIX_EPOCH)?.as_secs();
    let mut genesis = load_chain_spec().inner.genesis.clone();
    genesis.timestamp = genesis_time;
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));
    let chain_id = chain_spec.inner.chain.id();
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
    let system_config = SystemConfig {
        gas_limit: chain_spec.inner.genesis.gas_limit,
        eip1559_denominator: Some(50),
        eip1559_elasticity: Some(6),
        ..Default::default()
    };
    let canonical = RollupConfig {
        l2_chain_id: chain_id.into(),
        block_time: LEGACY_BLOCK_TIME,
        genesis: ChainGenesis {
            l1: l1_info.id(),
            l2: BlockNumHash { number: 0, hash: chain_spec.inner.genesis_hash() },
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
    };

    // Build a pre-Denim snapshot head directly.
    let genesis_hash = canonical.genesis.l2.hash;
    let attributes =
        StandaloneAttributesBuilder::new(Arc::new(canonical.clone()), l1_info, system_config, None)
            .prepare_payload_attributes(
                L2BlockInfo::new(
                    BlockInfo::new(genesis_hash, 0, B256::ZERO, genesis_time),
                    l1_info.id(),
                    l1_info.sequence_number(),
                ),
                l1_info.id(),
            )
            .await?;
    let engine = EngineConfig {
        config: Arc::new(canonical.clone()),
        l2_url: engine_url.clone(),
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

    let datadir = builder.datadir().to_path_buf();
    let state_path = datadir.join(StandaloneDevChain::STATE_FILE);
    let start = |token: CancellationToken| -> Result<JoinHandle<_>> {
        // Each start models a new process, which has no runtime schedule until it opens the state.
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        let chain = StandaloneDevChain::open(&datadir, canonical.clone())?;
        Ok(tokio::spawn(chain.run(engine_url.clone(), jwt_secret, token)))
    };

    let token = CancellationToken::new();
    let first = start(token.clone())?;
    let included = send_transaction(&provider, chain_id).await?;
    token.cancel();
    first.await??;
    let activation = genesis_time + 2 * LEGACY_BLOCK_TIME;
    let state: StandaloneDevState = serde_json::from_slice(&std::fs::read(&state_path)?)?;
    assert_eq!(state.boundary, BlockNumHash { number: 1, hash: boundary_hash });
    assert_eq!(state.denim_timestamp, activation);
    let included = block(&provider, included).await?;
    assert!(
        BaseTimeUpdateTx::extract_from_transactions(&included.body.transactions, included.number)
            .is_ok()
    );
    assert_eq!(included.header.gas_limit, system_config.gas_limit / 10);

    // A restart continues the persisted schedule without another activation or gas rescaling.
    let head = provider.get_block_number().await?;
    let token = CancellationToken::new();
    let second = start(token.clone())?;
    wait_for_block(&provider, head + 3).await?;
    token.cancel();
    second.await??;
    let next = block(&provider, head + 1).await?;
    assert_eq!(next.header.gas_limit, system_config.gas_limit / 10);
    assert_eq!(
        BaseTimeUpdateTx::extract_timestamp_ms(
            &next.body.transactions,
            head + 1,
            next.header.timestamp
        )?,
        canonical_millis(activation, head + 1)
    );

    // Without the state, development blocks are detected instead of being treated as a snapshot.
    std::fs::remove_file(&state_path)?;
    let error = start(CancellationToken::new())?.await?.unwrap_err();
    assert!(matches!(error, StandaloneDevError::MissingState { .. }), "{error}");

    builder.shutdown().await
}

/// Timestamp of a block after Denim activates at block 2.
const fn canonical_millis(activation: u64, number: u64) -> u64 {
    activation * 1_000 + (number - 2) * 200
}

async fn send_transaction(provider: &RootProvider<Base>, chain_id: u64) -> Result<u64> {
    let signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)?;
    let transaction = BaseTransactionRequest::default()
        .from(signer.address())
        .to(Address::repeat_byte(0xfe))
        .value(U256::from(1))
        .transaction_type(2)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(2_000_000_000)
        .with_max_priority_fee_per_gas(1_000_000)
        .with_chain_id(chain_id)
        .with_nonce(provider.get_transaction_count(signer.address()).await?)
        .build_typed_tx()
        .map_err(|error| eyre::eyre!("invalid transaction: {error:?}"))?;
    let signature = signer.sign_hash_sync(&transaction.signature_hash())?;
    let raw: Bytes = transaction.into_signed(signature).encoded_2718().into();
    let hash = *provider.send_raw_transaction(&raw).await?.tx_hash();
    timeout(Duration::from_secs(20), async {
        loop {
            if let Some(receipt) = provider.get_transaction_receipt(hash).await? {
                assert!(receipt.status());
                return receipt.inner.block_number.ok_or_eyre("receipt missing block number");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?
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
