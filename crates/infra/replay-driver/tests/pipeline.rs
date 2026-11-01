//! Production forwarding, recording, export, and replay against in-process canonical/shadow chains.

use std::time::{Duration, UNIX_EPOCH};

use alloy_eips::Encodable2718;
use alloy_provider::ProviderBuilder;
use base_builder_core::{
    BuilderApiExtension, BuilderApiExtensionConfig, BuilderConfig,
    test_utils::{LocalInstance, LocalInstanceBuilder, default_node_config},
};
use base_clock::{GlobalClock, OffsetClock};
use base_common_network::Base;
use base_execution_txpool::{NoExtensions, ValidatedTransaction};
use base_replay_driver::{
    BuilderIngressInjector, InjectionSchedule, ReplayDataset, ReplayRunner, ShadowDriver,
};
use base_replay_export::{ExportWindow, ExportWriter};
use base_replay_recorder::Recorder;
use base_replay_store::{ReplayStore, ReplayWriter};
use base_tx_forwarding::{InsertValidatedTransaction, TxForwardingConfig, TxForwardingService};
use reth_tasks::{RuntimeBuilder, RuntimeConfig, TokioConfig};
use tokio::{net::TcpListener, sync::mpsc, task::JoinSet, time::timeout};
use tokio_util::sync::CancellationToken;
use url::Url;

#[tokio::test(flavor = "multi_thread")]
async fn reproduces_canonical_transactions_when_replaying_forwarded_capture() -> eyre::Result<()> {
    let canonical = LocalInstance::flashblocks().await?;
    let source = canonical.driver().await?;
    let dir = tempfile::tempdir()?;
    let store = ReplayStore::Local(dir.path().join("store"));
    let writer = ReplayWriter::new(store.clone(), "test".into())?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let recorder_url: Url = format!("http://{}", listener.local_addr()?).parse()?;
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let mut tasks = JoinSet::new();
    tasks.spawn(Recorder::new(writer, cancel.clone()).serve(listener));
    let runtime = RuntimeBuilder::new(
        RuntimeConfig::default()
            .with_tokio(TokioConfig::existing_handle(tokio::runtime::Handle::current())),
    )
    .build()?;

    // Capture uses the default real-time clock. Drain each production forwarding queue before
    // building its canonical block. The helper forces transactions into future-timestamp blocks;
    // submit the next transaction only after that deadline, or replay can correctly include it early.
    let mut blocks = Vec::new();
    let mut hashes = Vec::new();
    let mut next_arrival_ms = 0;
    for nonce in 0..3 {
        tokio::time::sleep(InjectionSchedule::delay(next_arrival_ms, GlobalClock::get().now())?)
            .await;
        let tx = source.create_transaction().with_nonce(nonce).build().await;
        let raw = tx.encoded_2718().into();
        let request = InsertValidatedTransaction {
            transaction: ValidatedTransaction {
                sender: tx.signer(),
                raw,
                metering: None,
                extensions: NoExtensions {},
            },
            tx_hash: tx.tx_hash(),
        };
        let (sender, receiver) = mpsc::channel(1);
        let forwarding =
            TxForwardingService::new(TxForwardingConfig::new(vec![recorder_url.clone()]))
                .spawn_requests(vec![(recorder_url.clone(), receiver)], &runtime)?;
        sender.send(request).await?;
        drop(sender);
        let shutdown = timeout(Duration::from_secs(10), forwarding.shutdown()).await?;
        assert_eq!(shutdown.forwarders_completed, 1);
        assert_eq!(shutdown.task_failures, 0);
        assert!(!shutdown.timed_out);
        let timestamp = u64::try_from(GlobalClock::get().now_unix_millis()?)? / 1000 + 3;
        blocks.push(
            source
                .build_new_block_with_txs_timestamp(
                    vec![tx.encoded_2718().into()],
                    None,
                    Some(Duration::from_secs(timestamp)),
                    None,
                    Some(0),
                )
                .await?,
        );
        next_arrival_ms = blocks.last().expect("built canonical block").header.timestamp * 1000;
        hashes.push(tx.tx_hash());
    }
    cancel.cancel();
    timeout(Duration::from_secs(10), tasks.join_next()).await?.expect("recorder task")??;

    let provider = canonical.provider().await?;
    let export_rpc =
        ProviderBuilder::new().connect_ipc(canonical.rpc_ipc().to_owned().into()).await?;
    let window = ExportWindow::resolve(&export_rpc, 1..=3, Duration::from_secs(8)).await?;
    let out = dir.path().join("export");
    let manifest = ExportWriter { store, network: "test".into() }.export(window, &out).await?;
    assert_eq!(manifest.record_count, u64::try_from(hashes.len())?);
    let dataset = ReplayDataset::load(&out).await?;
    assert_eq!(dataset.records.len(), hashes.len());
    for ((record, hash), block) in dataset.records.iter().zip(&hashes).zip(&blocks) {
        println!(
            "capture: received_at_ms={} canonical_timestamp_ms={} tx_hash={hash}",
            record.received_at_ms,
            block.header.timestamp * 1000
        );
        assert_eq!(record.method, "base_insertValidatedTransaction");
        assert_eq!(record.tx_hash, Some(*hash));
        assert!(record.received_at_ms < block.header.timestamp * 1000);
        assert!(block.transactions.hashes().any(|actual| actual == *hash));
    }
    for (record, parent) in dataset.records.iter().skip(1).zip(&blocks) {
        assert!(record.received_at_ms >= parent.header.timestamp * 1000);
    }

    // Only shadow startup/replay uses the offset clock. Its startup allowance puts all recorded
    // arrivals and canonical deadlines in the future; runner and builder share this exact clock.
    GlobalClock::set(OffsetClock::new(
        UNIX_EPOCH + Duration::from_millis(dataset.clock_start_ms()),
    ))?;
    let reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = reservation.local_addr()?;
    let mut config = default_node_config();
    config.rpc.http = true;
    config.rpc.http_addr = address.ip();
    config.rpc.http_port = address.port();
    config.rpc.http_api = Some("eth".parse()?);
    drop(reservation);
    let shadow = LocalInstanceBuilder::new(BuilderConfig::for_tests())
        .with_node_config(config)
        .install_ext::<BuilderApiExtension>(
            BuilderApiExtensionConfig::default().with_noop_metering(),
        )
        .build()
        .await?;
    let engine = ProviderBuilder::new_with_network::<Base>()
        .connect_ipc(shadow.auth_ipc().to_owned().into())
        .await?;
    let injector = BuilderIngressInjector::new(format!("http://{address}").parse()?, &runtime)?;
    let runner = ReplayRunner {
        canonical: provider,
        engine,
        injector,
        chain: shadow.node_config().chain.as_ref(),
        driver: ShadowDriver::default(),
    };

    let report = timeout(Duration::from_secs(60), runner.run(dataset)).await??;

    println!("{}", report.markdown());
    assert_eq!(report.parent_block_number, 0);
    assert_eq!(report.blocks.iter().map(|block| block.block_number).collect::<Vec<_>>(), [1, 2, 3]);
    for (index, (diff, block)) in report.blocks.iter().zip(&blocks).enumerate() {
        assert_eq!(diff.warmup, index == 0);
        // Set comparison covers every canonical hash, including the forwarded transaction and
        // forced L1-info deposit. Check warmup too, not just the scored aggregate.
        assert_eq!((diff.totals.missing, diff.totals.extra), (0, 0));
        assert_eq!(diff.totals.common, block.transactions.len());
        assert_eq!(diff.totals.builder_missing, Some(0));
    }
    assert_eq!((report.aggregate.missing, report.aggregate.extra), (0, 0));
    assert_eq!(report.aggregate.builder_missing, Some(0));
    assert_eq!(
        report.aggregate.common,
        blocks.iter().skip(1).map(|block| block.transactions.len()).sum::<usize>()
    );
    assert_eq!(shadow.driver().await?.latest().await?.header.hash, blocks[2].header.hash);
    Ok(())
}
