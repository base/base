//! Production forwarding transport against the bounded recorder.

use std::time::Duration;

use base_clock::GlobalClock;
use base_replay_driver::{BuilderIngressInjector, Injector};
use base_replay_recorder::{CaptureContext, Recorder, RpcEnvelope};
use base_replay_store::{ReplayStore, ReplayWriter};
use futures::TryStreamExt;
use reth_tasks::{RuntimeBuilder, RuntimeConfig, TokioConfig};
use tokio::{net::TcpListener, task::JoinSet, time::timeout};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn accepts_unit_results_when_real_forwarder_delivers() {
    let dir = tempfile::tempdir().unwrap();
    let store = ReplayStore::Local(dir.path().into());
    let writer = ReplayWriter::new(store.clone(), "devnet".into()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let cancel = CancellationToken::new();
    let mut tasks = JoinSet::new();
    tasks.spawn(Recorder::new(writer, cancel.clone()).serve(listener));
    let runtime = RuntimeBuilder::new(
        RuntimeConfig::default()
            .with_tokio(TokioConfig::existing_handle(tokio::runtime::Handle::current())),
    )
    .build()
    .unwrap();
    let fixture =
        include_bytes!("../../../utilities/replay/testdata/insert-validated-transaction.json");
    let context =
        CaptureContext { received_at_ms: 0, source: "127.0.0.1:1".parse().unwrap(), batch_id: 0 };
    let (_, record) =
        RpcEnvelope::parse(fixture).unwrap().into_records(&context).pop().unwrap().unwrap();
    let params = record.params.get().to_owned();
    let hash = record.tx_hash;
    let start = u64::try_from(GlobalClock::get().now_unix_millis().unwrap()).unwrap();
    let injector = BuilderIngressInjector::new(url.parse().unwrap(), &runtime).unwrap();

    injector.inject(record).await.unwrap();
    drop(injector);
    cancel.cancel();
    timeout(Duration::from_secs(10), tasks.join_next()).await.unwrap().unwrap().unwrap().unwrap();

    let end = u64::try_from(GlobalClock::get().now_unix_millis().unwrap()).unwrap();
    let records: Vec<_> = store.read("devnet", start..=end).try_collect().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].params.get(), params);
    assert_eq!(records[0].tx_hash, hash);
}
