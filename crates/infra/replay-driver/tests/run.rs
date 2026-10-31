//! Full export replay through canonical RPC, ingress HTTP, and a real shadow builder.

use std::time::{Duration, UNIX_EPOCH};

use alloy_eips::Encodable2718;
use alloy_provider::{Provider, ProviderBuilder};
use base_builder_core::test_utils::LocalInstance;
use base_clock::{GlobalClock, OffsetClock};
use base_common_network::Base;
use base_replay::{ReplayRecord, ReplayStream};
use base_replay_driver::{BuilderIngressInjector, ReplayDataset, ReplayRunner, ShadowDriver};
use base_replay_export::{ExportManifest, ExportWindow};
use jsonrpsee::{RpcModule, server::Server, types::ErrorObjectOwned};
use reth_tasks::{RuntimeBuilder, RuntimeConfig, TokioConfig};
use serde_json::value::RawValue;

#[tokio::test(flavor = "multi_thread")]
async fn produces_diff_when_replaying_export_against_real_builder() -> eyre::Result<()> {
    let canonical = LocalInstance::flashblocks().await?;
    let source = canonical.driver().await?;
    let mut blocks = Vec::new();
    let mut transactions = Vec::new();
    for nonce in 0..3 {
        let tx = source.create_transaction().with_nonce(nonce).build().await;
        blocks.push(source.build_new_block_with_txs(vec![tx.encoded_2718().into()]).await?);
        transactions.push(tx);
    }
    let from = blocks[0].header.timestamp * 1000;
    GlobalClock::set(OffsetClock::new(UNIX_EPOCH + Duration::from_millis(from - 2000)))?;
    let shadow = LocalInstance::flashblocks().await?;
    let provider = shadow.provider().await?;
    let mut rpc = RpcModule::new(provider);
    rpc.register_async_method("eth_sendRawTransaction", |params, provider, _| async move {
        let raw: Box<RawValue> = params.parse()?;
        provider
            .raw_request::<_, Box<RawValue>>("eth_sendRawTransaction".into(), raw)
            .await
            .map_err(|error| ErrorObjectOwned::owned(-32000, error.to_string(), None::<()>))
    })?;
    let server = Server::builder().build("127.0.0.1:0").await?;
    let url = format!("http://{}", server.local_addr()?).parse()?;
    let server = server.start(rpc);
    let runtime = RuntimeBuilder::new(
        RuntimeConfig::default()
            .with_tokio(TokioConfig::existing_handle(tokio::runtime::Handle::current())),
    )
    .build()?;
    let injector = BuilderIngressInjector::new(url, &runtime)?;
    let records = transactions
        .iter()
        .enumerate()
        .map(|(index, tx)| {
            Ok(ReplayRecord {
                stream: ReplayStream::BuilderIngress,
                received_at_ms: if index == 0 {
                    from - 2000
                } else {
                    blocks[index - 1].header.timestamp * 1000 + 100
                },
                method: "eth_sendRawTransaction".into(),
                params: RawValue::from_string(serde_json::to_string(&[
                    alloy_primitives::Bytes::from(tx.encoded_2718()),
                ])?)?,
                batch_id: None,
                source: "test".into(),
                tx_hash: Some(tx.tx_hash()),
            })
        })
        .collect::<eyre::Result<Vec<_>>>()?;
    let manifest = ExportManifest {
        format: "replay-export/v1".into(),
        record_format: "replay-record/v1".into(),
        network: "test".into(),
        record_count: 3,
        window: ExportWindow {
            chain_id: 901,
            from_block: 1,
            to_block: 3,
            from_timestamp_ms: from,
            to_timestamp_ms: blocks[2].header.timestamp * 1000,
            warmup_ms: 8000,
            start_ms: from - 8000,
            end_ms: blocks[2].header.timestamp * 1000 + 2000,
        },
    };
    let engine = ProviderBuilder::new_with_network::<Base>()
        .connect_ipc(shadow.auth_ipc().to_owned().into())
        .await?;
    let runner = ReplayRunner {
        canonical: canonical.provider().await?,
        engine,
        injector,
        chain: shadow.node_config().chain.as_ref(),
        driver: ShadowDriver::default(),
    };

    let report = runner.run(ReplayDataset::new(manifest, records)?).await?;

    assert_eq!(report.blocks.len(), 3);
    assert!(report.blocks[0].warmup);
    assert_eq!((report.aggregate.missing, report.aggregate.extra), (0, 0));
    assert_eq!(report.aggregate.builder_missing, Some(0));
    assert_eq!(shadow.driver().await?.latest().await?.header.hash, blocks[2].header.hash);
    println!("full replay run:\n{}", report.markdown());
    server.stop()?;
    Ok(())
}
