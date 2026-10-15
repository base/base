//! Real-stack coverage for persisting and restoring a standalone development chain.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_consensus::SignableTransaction;
use alloy_eips::{
    BlockNumHash, BlockNumberOrTag,
    eip2718::{Decodable2718, Encodable2718},
};
use alloy_genesis::GenesisAccount;
use alloy_network::{ReceiptResponse, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Network, Provider, RootProvider, transport::TransportResult};
use alloy_rpc_types_engine::{ForkchoiceState, JwtSecret};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use axum::{Router, http::HeaderMap};
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_genesis::{
    ChainGenesis, RollupConfig, RuntimeUpgradeRegistry, SystemConfig, UpgradeConfig,
};
use base_common_network::{Base, BaseEngineApi};
use base_common_rpc_types::BaseTransactionRequest;
use base_consensus_derive::AttributesBuilder;
use base_consensus_node::{
    EngineConfig, NodeOperatingMode, StandaloneAttributesBuilder, StandaloneDevChain,
    StandaloneDevError, StandaloneDevState, StandaloneImpersonationQueue,
};
use base_execution_chainspec::BaseChainSpec;
use base_execution_txpool::ValiditySignatureMode;
use base_node_runner::{
    BaseNodeExtension, NodeHooks,
    test_utils::{L1_BLOCK_INFO_DEPOSIT_TX, load_chain_spec},
};
use base_protocol::{BaseTimeUpdateTx, BlockInfo, L1BlockInfoTx, L2BlockInfo};
use base_system_tests::{
    ANVIL_ACCOUNT_1, InProcessBuilder, InProcessBuilderConfig, InProcessNodeRuntime,
};
use eyre::{OptionExt, Result};
use reth_provider::{
    HeaderProvider, StaticFileProviderFactory, StaticFileSegment, StaticFileWriter,
};
use serde_json::{Value, json};
use tokio::{task::JoinHandle, time::timeout};
use tokio_util::sync::CancellationToken;
use url::Url;

const SNAPSHOT_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);
const LEGACY_BLOCK_TIME: u64 = 2;
const BOUNDARY_NUMBER: u64 = 8;
/// Contract that stores its first calldata word in slot 0 only when called by [`OWNER`].
const GUARDED: Address = Address::repeat_byte(0xc0);
/// Owner of [`GUARDED`], whose private key is unknown.
const OWNER: Address = Address::repeat_byte(0x0a);

#[derive(Debug)]
struct ImpersonationRpcExtension(StandaloneImpersonationQueue);

impl BaseNodeExtension for ImpersonationRpcExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.add_rpc_module(move |ctx| {
            ctx.modules.merge_configured(self.0.into_rpc())?;
            Ok(())
        })
    }
}

/// Persists the schedule chosen for a pre-Denim head, restores it after restarts, and sequences
/// impersonated deposits beside signed transactions.
#[tokio::test]
async fn standalone_dev_chain_persists_and_restores_schedule() -> Result<()> {
    base_node_runner::test_utils::init_silenced_tracing();
    let genesis_time = (SystemTime::now() - SNAPSHOT_AGE).duration_since(UNIX_EPOCH)?.as_secs();
    let mut genesis = load_chain_spec().inner.genesis.clone();
    genesis.timestamp = genesis_time;
    // CALLER == OWNER ? sstore(0, calldataload(0)) : revert(0, 0)
    let guarded_code = [
        &[0x33, 0x73][..],
        OWNER.as_slice(),
        &[
            0x14, 0x60, 0x1e, 0x57, 0x60, 0x00, 0x80, 0xfd, 0x5b, 0x60, 0x00, 0x35, 0x60, 0x00,
            0x55, 0x00,
        ],
    ]
    .concat();
    genesis.alloc.insert(GUARDED, GenesisAccount::default().with_code(Some(guarded_code.into())));
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));
    let chain_id = chain_spec.inner.chain.id();
    let jwt_secret = JwtSecret::random();
    let queue = StandaloneImpersonationQueue::default();
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
        basic_payload_builder: false,
        validity_signature_mode: ValiditySignatureMode::Off,
        extra_extensions: vec![Box::new(ImpersonationRpcExtension(queue.clone()))],
        block_time: Duration::from_millis(200),
        persistence_threshold: Some(0),
        persistence_backpressure_threshold: None,
        txpool_max_transactions: None,
        txpool_max_size_mb: None,
        txpool_max_account_slots: None,
    })
    .await?;
    let provider = RootProvider::<Base>::new_http(builder.rpc_url()?);
    let engine_url = builder.engine_url()?;
    let fund = |address: Address| {
        json!({
            "from": address,
            "to": address,
            "gas": "0x5208",
            "mint": "0x64",
        })
    };
    let not_ready = impersonate(&provider, fund(OWNER)).await.unwrap_err();
    assert_eq!(not_ready.as_error_resp().map(|error| error.code), Some(-32000));

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

    let genesis_hash = canonical.genesis.l2.hash;
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
    let mut parent = L2BlockInfo::new(
        BlockInfo::new(genesis_hash, 0, B256::ZERO, genesis_time),
        l1_info.id(),
        l1_info.sequence_number(),
    );
    let mut attributes_builder =
        StandaloneAttributesBuilder::new(Arc::new(canonical.clone()), l1_info, system_config, None);
    // Move beyond the engine's in-memory buffer so history expiry uses persisted transactions.
    for number in 1..=BOUNDARY_NUMBER {
        let attributes =
            attributes_builder.prepare_payload_attributes(parent, l1_info.id()).await?;
        let payload_id = engine
            .fork_choice_updated_v3(fork_choice(parent.block_info.hash), Some(attributes))
            .await?
            .payload_id
            .ok_or_eyre("boundary build did not start")?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let payload = engine.get_payload_v4(payload_id).await?.execution_payload;
        let status = engine.new_payload_v4(payload, B256::ZERO).await?;
        let hash = status.latest_valid_hash.ok_or_eyre("boundary payload rejected")?;
        engine.fork_choice_updated_v3(fork_choice(hash), None).await?;
        parent = L2BlockInfo::from_block_and_genesis(
            &block(&provider, number).await?,
            &canonical.genesis,
        )?;
    }
    let boundary_hash = parent.block_info.hash;

    // Model a snapshot whose retained transaction history starts after genesis.
    let static_files = builder.provider.static_file_provider();
    timeout(Duration::from_secs(20), async {
        while static_files
            .get_highest_static_file_block(StaticFileSegment::Transactions)
            .unwrap_or_default()
            < 1
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let mut writer = static_files.latest_writer(StaticFileSegment::Transactions)?;
    let end = writer.user_header().block_end().ok_or_eyre("no persisted transaction history")?;
    writer.user_header_mut().set_block_range(1, end);
    writer.inner().set_dirty();
    writer.commit()?;
    drop(writer);
    static_files.initialize_index()?;
    assert_eq!(static_files.earliest_history_height(), 1);
    assert!(provider.get_block_by_number(0.into()).full().await.is_err());

    // Fail one seal with pending deposits, then lose an acknowledgment after canonical minting.
    let funded = Address::repeat_byte(0xf2);
    let lost_ack = Arc::new(AtomicBool::new(false));
    let fault = Arc::clone(&lost_ack);
    let failed_build = Arc::new(AtomicBool::new(false));
    let build_fault = Arc::clone(&failed_build);
    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
    let reader = provider.clone();
    let app = Router::new().fallback(move |headers: HeaderMap, body: String| {
        let (client, target, reader, fault) =
            (client.clone(), engine_url.clone(), reader.clone(), Arc::clone(&fault));
        let build_fault = Arc::clone(&build_fault);
        async move {
            let request: Value = serde_json::from_str(&body).unwrap();
            let mut response: Value = client
                .post(target)
                .headers(headers)
                .json(&request)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if request["method"] == "engine_getPayloadV4"
                && response["result"]["executionPayload"]["transactions"].as_array().is_some_and(
                    |transactions| {
                        transactions.iter().any(|transaction| {
                            let encoded: Bytes =
                                serde_json::from_value(transaction.clone()).unwrap();
                            BaseTxEnvelope::decode_2718(&mut encoded.as_ref())
                                .unwrap()
                                .as_deposit()
                                .is_some_and(|deposit| deposit.from == funded)
                        })
                    },
                )
                && !build_fault.swap(true, Ordering::SeqCst)
            {
                response = json!({
                    "jsonrpc": "2.0", "id": request["id"],
                    "error": {"code": -38001, "message": "injected lost payload"}
                });
            } else if request["method"] == "engine_forkchoiceUpdatedV3"
                && request["params"][1].is_null()
                && response["result"]["payloadStatus"]["status"] == "VALID"
                && !fault.load(Ordering::SeqCst)
                && reader.get_balance(funded).await.unwrap() > U256::ZERO
                && !fault.swap(true, Ordering::SeqCst)
            {
                response = json!({
                    "jsonrpc": "2.0", "id": request["id"],
                    "error": {"code": -38002, "message": "injected lost insertion acknowledgment"}
                });
            }
            ([("Content-Type", "application/json")], response.to_string())
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let engine_url = Url::parse(&format!("http://{}", listener.local_addr()?))?;
    let proxy = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let datadir = builder.datadir().to_path_buf();
    let state_path = datadir.join(StandaloneDevChain::STATE_FILE);
    let start = |token: CancellationToken| -> Result<JoinHandle<_>> {
        // Each start models a new process, which has no runtime schedule until it opens the state.
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        let chain = StandaloneDevChain::open(&datadir, canonical.clone())?;
        let headers = builder.provider.clone();
        Ok(tokio::spawn(chain.run(
            engine_url.clone(),
            jwt_secret,
            move |number| headers.header_by_number(number),
            queue.clone(),
            token,
        )))
    };

    // Reject an altered schedule even if the process stopped before its first development block.
    let activation = genesis_time + (BOUNDARY_NUMBER + 1) * LEGACY_BLOCK_TIME;
    StandaloneDevState {
        l2_chain_id: chain_id,
        genesis_hash,
        boundary: BlockNumHash { number: BOUNDARY_NUMBER, hash: boundary_hash },
        denim_timestamp: activation + LEGACY_BLOCK_TIME,
    }
    .persist(&state_path)?;
    let token = CancellationToken::new();
    let mut altered = start(token.clone())?;
    let rejected = timeout(Duration::from_secs(20), &mut altered).await;
    token.cancel();
    if rejected.is_err() {
        altered.await??;
    }
    assert!(
        matches!(rejected, Ok(Ok(Err(StandaloneDevError::StateMismatch { .. })))),
        "altered boundary schedule was accepted: {rejected:?}"
    );
    std::fs::remove_file(&state_path)?;

    let token = CancellationToken::new();
    let first = start(token.clone())?;
    let included = send_transaction(&provider, chain_id).await?;
    // Requests execute in submission order after the system deposits.
    let call = |from: Address, word: u8, gas: &str| {
        json!({
            "from": from,
            "to": GUARDED,
            "input": B256::with_last_byte(word),
            "gas": gas,
        })
    };
    let mints =
        [impersonate(&provider, fund(funded)).await?, impersonate(&provider, fund(funded)).await?];
    assert_ne!(mints[0], mints[1]);
    let authorized = impersonate(&provider, call(OWNER, 42, "0x186a0")).await?;
    let unauthorized =
        impersonate(&provider, call(Address::repeat_byte(0x0b), 7, "0x186a0")).await?;
    // Covers intrinsic gas but halts before SSTORE.
    let halted = impersonate(&provider, call(OWNER, 9, "0x55f0")).await?;
    // Init code returning the one-byte runtime code 0xff.
    let deployer = Address::repeat_byte(0xd0);
    let created = impersonate(
        &provider,
        json!({ "from": deployer, "input": "0x60ff60005360016000f3", "gas": "0x186a0" }),
    )
    .await?;
    for (hash, success) in [
        (mints[0], true),
        (mints[1], true),
        (authorized, true),
        (unauthorized, false),
        (halted, false),
        (created, true),
    ] {
        assert_eq!(receipt(&provider, hash).await?.status(), success);
    }
    wait_for_block(&provider, provider.get_block_number().await? + 2).await?;
    assert!(failed_build.load(Ordering::SeqCst), "build did not fail with pending deposits");
    assert!(lost_ack.load(Ordering::SeqCst), "insertion acknowledgment was not lost");
    assert!(queue.select(u64::MAX).is_empty());
    assert_eq!(provider.get_balance(funded).await?, U256::from(0xc8));
    assert_eq!(provider.get_storage_at(GUARDED, U256::ZERO).await?, U256::from(42));
    assert_eq!(provider.get_code_at(deployer.create(0)).await?, Bytes::from_static(&[0xff]));
    let max_gas = system_config.gas_limit / 10 - 2_000_000;
    for request in [
        json!({ "from": OWNER, "to": GUARDED, "gas": format!("{:#x}", max_gas + 1) }),
        json!({ "from": OWNER, "to": GUARDED, "gas": "0x4e20" }),
        json!({
            "from": OWNER,
            "to": GUARDED,
            "gas": format!("{max_gas:#x}"),
            "input": Bytes::from(vec![1; StandaloneImpersonationQueue::MAX_INPUT_BYTES + 1]),
        }),
    ] {
        let error = impersonate(&provider, request).await.unwrap_err();
        assert_eq!(error.as_error_resp().map(|error| error.code), Some(-32602), "{error}");
    }
    token.cancel();
    first.await??;
    let state: StandaloneDevState = serde_json::from_slice(&std::fs::read(&state_path)?)?;
    assert_eq!(state.boundary, BlockNumHash { number: BOUNDARY_NUMBER, hash: boundary_hash });
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

    proxy.abort();
    builder.shutdown().await
}

/// Timestamp of a block after Denim activates at the snapshot boundary's next slot.
const fn canonical_millis(activation: u64, number: u64) -> u64 {
    activation * 1_000 + (number - BOUNDARY_NUMBER - 1) * 200
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
    let receipt = receipt(provider, hash).await?;
    assert!(receipt.status());
    receipt.block_number().ok_or_eyre("receipt missing block number")
}

async fn impersonate(provider: &RootProvider<Base>, request: Value) -> TransportResult<B256> {
    provider.raw_request(StandaloneImpersonationQueue::METHOD.into(), (request,)).await
}

async fn receipt(
    provider: &RootProvider<Base>,
    hash: B256,
) -> Result<<Base as Network>::ReceiptResponse> {
    timeout(Duration::from_secs(20), async {
        loop {
            if let Some(receipt) = provider.get_transaction_receipt(hash).await? {
                return Ok(receipt);
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
