//! Candidate-only transactions must survive canonical re-anchoring and unblock later nonces.

use std::time::{Duration, UNIX_EPOCH};

use alloy_consensus::TxEip1559;
use alloy_eips::Encodable2718;
use alloy_provider::{Provider, ProviderBuilder};
use base_builder_core::{
    BuilderApiExtension, BuilderApiExtensionConfig, BuilderConfig,
    test_utils::{LocalInstance, LocalInstanceBuilder, funded_signer, sign_base_tx},
};
use base_clock::{GlobalClock, OffsetClock};
use base_common_consensus::BaseTypedTransaction;
use base_common_network::Base;
use base_execution_txpool::{NoExtensions, ValidatedTransaction};
use base_replay::{ReplayRecord, ReplayStream};
use base_replay_driver::{BuilderIngressInjector, ReplayDataset, ReplayRunner, ShadowDriver};
use base_replay_export::{ExportManifest, ExportWindow};
use jsonrpsee::{RpcModule, server::Server, types::ErrorObjectOwned};
use reth_tasks::{RuntimeBuilder, RuntimeConfig, TokioConfig};
use serde_json::value::RawValue;

#[tokio::test(flavor = "multi_thread")]
async fn preserves_next_block_and_followup_nonce_when_candidate_includes_transaction_early()
-> eyre::Result<()> {
    let canonical = LocalInstance::flashblocks().await?;
    let source = canonical.driver().await?;
    let [early, followup, obsolete, replacement, last] = [(0, 0), (1, 0), (2, 0), (2, 1), (3, 0)]
        .map(|(nonce, tip)| {
            sign_base_tx(
                &funded_signer(),
                BaseTypedTransaction::Eip1559(TxEip1559 {
                    chain_id: 901,
                    nonce,
                    gas_limit: 210_000,
                    max_fee_per_gas: 1_000_000_000,
                    max_priority_fee_per_gas: tip,
                    ..Default::default()
                }),
            )
            .expect("signed nonce fixture")
        });
    assert_eq!(early.signer(), followup.signer());
    assert_ne!(early.tx_hash(), followup.tx_hash());
    assert_ne!(obsolete.tx_hash(), replacement.tx_hash());
    let first = source.build_new_block_with_txs(vec![]).await?;
    let second = source
        .build_new_block_with_txs(vec![early.encoded_2718().into(), followup.encoded_2718().into()])
        .await?;
    let third = source.build_new_block_with_txs(vec![replacement.encoded_2718().into()]).await?;
    let fourth = source.build_new_block_with_txs(vec![last.encoded_2718().into()]).await?;
    let from = first.header.timestamp * 1000;
    assert_eq!(second.transactions.len(), 3, "deposit plus both sender nonces");
    let to = fourth.header.timestamp * 1000;
    GlobalClock::set(OffsetClock::new(UNIX_EPOCH + Duration::from_millis(from - 20_000)))?;
    let shadow = LocalInstanceBuilder::new(BuilderConfig::for_tests())
        .install_ext::<BuilderApiExtension>(
            BuilderApiExtensionConfig::default().with_noop_metering(),
        )
        .build()
        .await?;
    let mut rpc = RpcModule::new(shadow.provider().await?);
    rpc.register_async_method(
        "base_insertValidatedTransaction",
        |params, provider, _| async move {
            let raw: Box<RawValue> = params.parse()?;
            provider
                .raw_request::<_, Box<RawValue>>("base_insertValidatedTransaction".into(), raw)
                .await
                .map_err(|error| {
                    let error = error.as_error_resp().expect("builder RPC rejection");
                    ErrorObjectOwned::owned(
                        i32::try_from(error.code).expect("RPC code"),
                        error.message.as_ref(),
                        error.data.as_deref(),
                    )
                })
        },
    )?;
    let server = Server::builder().build("127.0.0.1:0").await?;
    let url = format!("http://{}", server.local_addr()?).parse()?;
    let server = server.start(rpc);
    let runtime = RuntimeBuilder::new(
        RuntimeConfig::default()
            .with_tokio(TokioConfig::existing_handle(tokio::runtime::Handle::current())),
    )
    .build()?;
    let records = [
        (&early, from - 2000),
        (&followup, from + 100),
        (&obsolete, second.header.timestamp * 1000 + 100),
        (&last, third.header.timestamp * 1000 + 100),
    ]
    .into_iter()
    .map(|(tx, received_at_ms)| {
        Ok(ReplayRecord {
            stream: ReplayStream::BuilderIngress,
            received_at_ms,
            method: "base_insertValidatedTransaction".into(),
            params: RawValue::from_string(serde_json::to_string(&[ValidatedTransaction {
                sender: tx.signer(),
                raw: tx.encoded_2718().into(),
                metering: None,
                extensions: NoExtensions {},
            }])?)?,
            batch_id: None,
            source: "pool-restore-test".into(),
            tx_hash: Some(tx.tx_hash()),
        })
    })
    .collect::<eyre::Result<Vec<_>>>()?;
    let manifest = ExportManifest {
        format: "replay-export/v1".into(),
        record_format: "replay-record/v1".into(),
        network: "test".into(),
        record_count: 4,
        window: ExportWindow {
            chain_id: 901,
            from_block: 1,
            to_block: 4,
            from_timestamp_ms: from,
            to_timestamp_ms: to,
            warmup_ms: 8000,
            start_ms: from - 8000,
            end_ms: to + 2000,
        },
    };
    let runner = ReplayRunner {
        canonical: canonical.provider().await?,
        engine: ProviderBuilder::new_with_network::<Base>()
            .connect_ipc(shadow.auth_ipc().to_owned().into())
            .await?,
        injector: BuilderIngressInjector::new(url, &runtime)?,
        chain: shadow.node_config().chain.as_ref(),
        driver: ShadowDriver::default(),
    };

    let report = tokio::time::timeout(
        Duration::from_secs(45),
        runner.run(ReplayDataset::new(manifest, records)?),
    )
    .await??;

    println!("candidate-only restoration:\n{}", report.markdown());
    assert_eq!(report.blocks[0].totals.extra, 1, "early nonce must execute in candidate K");
    assert_eq!(report.blocks[0].totals.missing, 0);
    assert_eq!(report.blocks[1].totals.missing, 0, "canonical K+1 needs restored nonce 0");
    assert_eq!(report.blocks[1].totals.extra, 0);
    assert_eq!(report.blocks[1].totals.common, second.transactions.len());
    assert!(second.transactions.hashes().any(|hash| hash == followup.tx_hash()));
    assert_eq!((report.blocks[2].totals.missing, report.blocks[2].totals.extra), (1, 1));
    assert_eq!((report.blocks[3].totals.missing, report.blocks[3].totals.extra), (0, 0));
    assert_eq!(report.blocks[3].totals.common, fourth.transactions.len());
    assert_eq!(shadow.driver().await?.latest().await?.header.hash, fourth.header.hash);
    server.stop()?;
    Ok(())
}
