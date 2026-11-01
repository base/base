//! Engine RPC completion advances the clock to expire exactly the middle build.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_consensus::Block;
use alloy_primitives::U256;
use alloy_provider::ProviderBuilder;
use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV2, ExecutionPayloadFieldV2, ForkchoiceState, ForkchoiceUpdated,
    PayloadId, PayloadStatus, PayloadStatusEnum,
};
use base_clock::{Clock, GlobalClock};
use base_common_consensus::BaseTxEnvelope;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_common_rpc_types::{BaseBlockResponse, BaseTransactionReceipt};
use base_common_rpc_types_engine::BaseExecutionPayload;
use base_replay_driver::{
    BuildWait, BuilderIngressInjector, ReplayDataset, ReplayRunner, ShadowDriver,
};
use base_replay_export::{ExportManifest, ExportWindow};
use jsonrpsee::{RpcModule, server::Server, types::ErrorObjectOwned};
use reth_tasks::{RuntimeBuilder, RuntimeConfig, TokioConfig};
use serde_json::Value;

#[derive(Debug)]
struct RpcClock(Arc<AtomicU64>);
impl Clock for RpcClock {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.0.load(Ordering::SeqCst))
    }
}

#[tokio::test]
async fn continues_scoring_when_middle_deadline_expires() -> eyre::Result<()> {
    let clock = Arc::new(AtomicU64::new(0));
    GlobalClock::set(RpcClock(Arc::clone(&clock)))?;
    let mut blocks = vec![Block::<BaseTxEnvelope>::default()];
    for number in 1..=3 {
        let mut block = Block::<BaseTxEnvelope>::default();
        block.header.number = number;
        block.header.timestamp = number;
        block.header.parent_hash = blocks.last().unwrap().hash_slow();
        block.header.gas_used = number * 10;
        blocks.push(block);
    }
    let calls = Arc::new(Mutex::new(Vec::new()));
    let imports = Arc::new(Mutex::new(Vec::new()));
    let mut rpc =
        RpcModule::new((blocks, Arc::clone(&clock), Arc::clone(&calls), Arc::clone(&imports)));
    rpc.register_method("eth_chainId", |_, _, _| "0x1")?;
    rpc.register_method("eth_getBlockByNumber", |params, ctx, _| {
        let (number, _): (String, bool) = params.parse()?;
        let number = usize::from_str_radix(&number[2..], 16).unwrap();
        let block = &ctx.0[number];
        let mut response: BaseBlockResponse = BaseBlockResponse::default();
        response.header.inner.inner = block.header.clone();
        response.header.hash = block.hash_slow();
        Ok::<_, ErrorObjectOwned>(response)
    })?;
    rpc.register_method("eth_getBlockReceipts", |_, _, _| None::<Vec<BaseTransactionReceipt>>)?;
    rpc.register_method("engine_forkchoiceUpdatedV2", |params, ctx, _| {
        let (state, attrs): (ForkchoiceState, Option<Value>) = params.parse()?;
        let number = ctx.0.iter().position(|b| b.hash_slow() == state.head_block_hash).unwrap();
        ctx.2.lock().unwrap().push((number, attrs.is_some()));
        if attrs.is_none() && number == 1 {
            ctx.1.store(2_000, Ordering::SeqCst);
        }
        Ok::<_, ErrorObjectOwned>(ForkchoiceUpdated {
            payload_status: PayloadStatus::from_status(PayloadStatusEnum::Valid),
            payload_id: attrs
                .map(|_| PayloadId::new(u64::try_from(number + 1).unwrap().to_be_bytes())),
        })
    })?;
    rpc.register_method("engine_getPayloadV2", |params, ctx, _| {
        let (id,): (PayloadId,) = params.parse()?;
        let number = usize::try_from(u64::from_be_bytes(*id.0)).unwrap();
        let (payload, _) = BaseExecutionPayload::from_block_slow(&ctx.0[number]);
        Ok::<_, ErrorObjectOwned>(ExecutionPayloadEnvelopeV2 {
            execution_payload: ExecutionPayloadFieldV2::V1(payload.as_v1().clone()),
            block_value: U256::ZERO,
        })
    })?;
    rpc.register_method("engine_newPayloadV2", |params, ctx, _| {
        let values: Vec<Value> = params.parse()?;
        ctx.3.lock().unwrap().push(values[0]["blockNumber"].clone());
        Ok::<_, ErrorObjectOwned>(PayloadStatus::from_status(PayloadStatusEnum::Valid))
    })?;
    let server = Server::builder().build("127.0.0.1:0").await?;
    let url: url::Url = format!("http://{}", server.local_addr()?).parse()?;
    let server = server.start(rpc);
    let runtime = RuntimeBuilder::new(
        RuntimeConfig::default()
            .with_tokio(TokioConfig::existing_handle(tokio::runtime::Handle::current())),
    )
    .build()?;
    let chain = RollupConfig::default();
    let runner = ReplayRunner {
        canonical: ProviderBuilder::new_with_network::<Base>().connect_http(url.clone()),
        engine: ProviderBuilder::new_with_network::<Base>().connect_http(url.clone()),
        injector: BuilderIngressInjector::new(url, &runtime)?,
        chain: &chain,
        driver: ShadowDriver { wait: BuildWait::For(Duration::ZERO), ..ShadowDriver::default() },
    };
    let dataset = ReplayDataset::new(
        ExportManifest {
            format: "replay-export/v1".into(),
            record_format: "replay-record/v1".into(),
            network: "test".into(),
            record_count: 0,
            window: ExportWindow {
                chain_id: 1,
                from_block: 1,
                to_block: 3,
                from_timestamp_ms: 1000,
                to_timestamp_ms: 3000,
                warmup_ms: 0,
                start_ms: 1000,
                end_ms: 3000 + ExportWindow::END_ALLOWANCE_MS,
            },
        },
        vec![],
    )?;

    let report = runner.run(dataset).await?;

    let json = serde_json::to_value(&report)?;
    assert_eq!(json["blocks"][1]["missed_deadline"], true);
    assert_eq!(json["missed_deadline_blocks"], 1);
    assert_eq!(report.blocks.len(), 3);
    assert_eq!(
        (report.aggregate.missing, report.aggregate.extra, report.aggregate.common),
        (0, 0, 0)
    );
    assert_eq!(report.aggregate.candidate_gas_used, 30);
    assert_eq!(*imports.lock().unwrap(), vec!["0x1", "0x2", "0x3"]);
    assert_eq!(
        *calls.lock().unwrap(),
        vec![(0, true), (1, false), (2, false), (2, true), (3, false)]
    );
    server.stop()?;
    Ok(())
}
