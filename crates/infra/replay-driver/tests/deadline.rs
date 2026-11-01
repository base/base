//! Canonical reads and SYNCING responses advance a controlled clock beyond the build deadline.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_provider::ProviderBuilder;
use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
use base_clock::{Clock, GlobalClock};
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_common_rpc_types::{BaseBlockResponse, BaseTransactionReceipt};
use base_replay::REPLAY_RECORD_FORMAT;
use base_replay_driver::{
    BuilderIngressInjector, ReplayDataset, ReplayRunner, ShadowDriver, ShadowError,
};
use base_replay_export::{ExportManifest, ExportWindow};
use jsonrpsee::{RpcModule, server::Server, types::ErrorObjectOwned};
use reth_tasks::{RuntimeBuilder, RuntimeConfig, TokioConfig};

/// Clock controlled by completion of the canonical receipts RPC.
#[derive(Debug)]
pub struct RpcClock(Arc<AtomicU64>);
impl Clock for RpcClock {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.0.load(Ordering::SeqCst))
    }
}

#[tokio::test]
async fn expired_deadlines_stop_initial_and_retried_build_fcus() -> eyre::Result<()> {
    let clock = Arc::new(AtomicU64::new(0));
    GlobalClock::set(RpcClock(Arc::clone(&clock)))?;
    let builds = Arc::new(AtomicU64::new(0));
    let parent: BaseBlockResponse = BaseBlockResponse::default();
    let mut first: BaseBlockResponse = BaseBlockResponse::default();
    first.header.number = 1;
    first.header.timestamp = 1;
    first.header.parent_hash = parent.header.inner.inner.hash_slow();
    let delay_receipts = Arc::new(AtomicBool::new(true));
    let mut rpc = RpcModule::new((
        vec![parent, first],
        Arc::clone(&clock),
        Arc::clone(&builds),
        Arc::clone(&delay_receipts),
    ));
    rpc.register_method("eth_chainId", |_, _, _| "0x1")?;
    rpc.register_method("eth_getBlockByNumber", |params, ctx, _| {
        let (number, _): (String, bool) = params.parse()?;
        let index = usize::from(number == "0x1");
        Ok::<_, ErrorObjectOwned>(ctx.0[index].clone())
    })?;
    rpc.register_async_method("eth_getBlockReceipts", |_, ctx, _| async move {
        tokio::task::yield_now().await;
        if ctx.3.load(Ordering::SeqCst) {
            ctx.1.store(2_000, Ordering::SeqCst);
        }
        None::<Vec<BaseTransactionReceipt>>
    })?;
    for method in ["engine_forkchoiceUpdatedV2", "engine_forkchoiceUpdatedV3"] {
        rpc.register_method(method, |_, ctx, _| {
            if ctx.2.fetch_add(1, Ordering::SeqCst) > 0 {
                return Err(ErrorObjectOwned::owned(-32000, "late build attempted", None::<()>));
            }
            ctx.1.store(2_000, Ordering::SeqCst);
            Ok(ForkchoiceUpdated {
                payload_status: PayloadStatus::from_status(PayloadStatusEnum::Syncing),
                payload_id: None,
            })
        })?;
    }
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
        driver: ShadowDriver::default(),
    };
    let dataset = || {
        ReplayDataset::new(
            ExportManifest {
                format: "replay-export/v1".into(),
                record_format: REPLAY_RECORD_FORMAT.into(),
                network: "test".into(),
                record_count: 0,
                window: ExportWindow {
                    chain_id: 1,
                    from_block: 1,
                    to_block: 1,
                    from_timestamp_ms: 1000,
                    to_timestamp_ms: 1000,
                    warmup_ms: 0,
                    start_ms: 1000,
                    end_ms: 3000,
                },
            },
            vec![],
        )
    };

    let error = runner.run(dataset()?).await.unwrap_err();

    assert_eq!(builds.load(Ordering::SeqCst), 0, "expired builds must not be sent");
    assert!(error.to_string().contains("deadline"), "{error}");

    clock.store(0, Ordering::SeqCst);
    delay_receipts.store(false, Ordering::SeqCst);

    let error = runner.run(dataset()?).await.unwrap_err();

    assert_eq!(builds.load(Ordering::SeqCst), 1, "expired retries must not be sent");
    assert!(
        matches!(error.downcast_ref::<ShadowError>(), Some(ShadowError::MissedDeadline(1))),
        "{error}"
    );
    server.stop()?;
    Ok(())
}
