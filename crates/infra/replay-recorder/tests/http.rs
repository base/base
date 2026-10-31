use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
};
use base_clock::GlobalClock;
use base_replay_recorder::{ArrivalQueue, Recorder};
use base_replay_store::{ReplayStore, ReplayWriter};
use futures::{TryStreamExt, stream};
use tokio::{net::TcpListener, task::JoinSet, time::timeout};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn rejects_admission_before_polling_body() {
    let (queue, _jobs) = ArrivalQueue::channel();
    let queue = Arc::new(queue);
    let source = "127.0.0.1:1".parse().unwrap();
    let _held: Vec<_> = (0..ArrivalQueue::CAPACITY).map(|_| queue.admit(source).unwrap()).collect();
    let polled = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&polled);
    let body = Body::from_stream(stream::once(async move {
        observed.store(true, Ordering::SeqCst);
        Err::<Bytes, _>(std::io::Error::other("body was polled"))
    }));

    let response = Recorder::record(State(queue), ConnectInfo(source), Request::new(body)).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(!polled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn records_valid_batch_members_with_exact_ids_and_multiline_params() {
    let dir = tempfile::tempdir().unwrap();
    let store = ReplayStore::Local(dir.path().into());
    let writer = ReplayWriter::new(store.clone(), "devnet".into()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let cancel = CancellationToken::new();
    let mut tasks = JoinSet::new();
    tasks.spawn(Recorder::new(writer, cancel.clone()).serve(listener));
    let client = reqwest::Client::builder().timeout(Duration::from_secs(5)).build().unwrap();
    let start = u64::try_from(GlobalClock::get().now_unix_millis().unwrap()).unwrap();
    let params = "[\r\n { \"n\": 1.2300e+02, \"s\": \"\\u0061\" }\n]";
    let call = format!(
        r#"{{"jsonrpc":"2.0","id":18446744073709551617,"method":"future","params":{params}}}"#
    );
    let success = r#"{"jsonrpc":"2.0","id":18446744073709551617,"result":null}"#;
    let batch = format!(r#"[{call},1,{{"jsonrpc":"2.0","method":"notification"}}]"#);

    for (body, expected) in
        [(call, success.to_owned()), (batch, format!("[{success},{}]", ArrivalQueue::INVALID))]
    {
        let response = client.post(&url).body(body).send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), expected);
    }
    let response =
        client.post(&url).body("x".repeat(Recorder::MAX_BODY_BYTES + 1)).send().await.unwrap();
    assert_eq!(response.status(), 413);
    assert_eq!(client.post(&url).body("[]").send().await.unwrap().status(), 400);
    cancel.cancel();
    timeout(Duration::from_secs(5), tasks.join_next()).await.unwrap().unwrap().unwrap().unwrap();

    let end = u64::try_from(GlobalClock::get().now_unix_millis().unwrap()).unwrap();
    let records: Vec<_> = store.read("devnet", start..=end).try_collect().await.unwrap();
    assert_eq!(
        records.iter().map(|r| r.method.as_str()).collect::<Vec<_>>(),
        ["future", "future", "notification"]
    );
    assert_eq!(records[0].params.get().as_bytes(), params.as_bytes());
    assert_eq!(records[1].params.get().as_bytes(), params.as_bytes());
    assert_eq!(records[0].batch_id, None);
    assert_eq!(records[1].batch_id, records[2].batch_id);
    assert!(records[1].batch_id.is_some());
    assert!(records.iter().all(|r| r.source.starts_with("127.0.0.1:")));
}
