use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use base_clock::GlobalClock;
use base_replay_store::{ReplayWriter, StoreError};
use serde_json::json;
use tokio::{net::TcpListener, sync::Mutex, time::timeout};
use tokio_util::sync::CancellationToken;

use crate::{CaptureContext, RpcEnvelope};

/// Serialized recording state. Batches never interleave other requests.
#[derive(Debug)]
pub struct Recorder {
    writer: ReplayWriter,
    next_batch: u64,
    shutdown: CancellationToken,
    failure: Option<StoreError>,
}

impl Recorder {
    /// Maximum HTTP body size, 8 `MiB`.
    pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

    /// Create one capture session owning its writer and shutdown token.
    pub const fn new(writer: ReplayWriter, shutdown: CancellationToken) -> Self {
        Self { writer, next_batch: 0, shutdown, failure: None }
    }

    /// Drain requests and close the writer on shutdown, returning server or storage errors.
    pub async fn serve(self, listener: TcpListener) -> Result<(), StoreError> {
        let shutdown = self.shutdown.clone();
        let state = Arc::new(Mutex::new(self));
        let app = Router::new().route("/", post(Self::record)).with_state(Arc::clone(&state));
        tracing::info!(listen = %listener.local_addr()?, "replay recorder listening");
        let served = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await;
        let mut state = state.lock().await;
        state.writer.close().await?;
        if let Some(error) = state.failure.take() {
            return Err(error);
        }
        Ok(served?)
    }

    /// Bound and parse the body before entering the ordered writer queue.
    pub async fn record(
        State(state): State<Arc<Mutex<Self>>>,
        ConnectInfo(source): ConnectInfo<SocketAddr>,
        request: Request,
    ) -> Response {
        let body = match timeout(
            Duration::from_secs(10),
            to_bytes(request.into_body(), Self::MAX_BODY_BYTES),
        )
        .await
        {
            Ok(Ok(body)) => body,
            Ok(Err(_)) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
            Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
        };
        let envelope = match RpcEnvelope::parse(&body) {
            Ok(envelope) => envelope,
            Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid request"}}))).into_response(),
        };
        let mut state = state.lock().await;
        if state.failure.is_some() {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        let received_at_ms =
            match GlobalClock::get().now_unix_millis().ok().and_then(|ms| u64::try_from(ms).ok()) {
                Some(ms) => ms,
                None => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            };
        let is_batch = matches!(&envelope, RpcEnvelope::Batch(_));
        let context = CaptureContext { received_at_ms, source, batch_id: state.next_batch };
        state.next_batch += u64::from(is_batch);
        let mut responses = Vec::new();
        for (id, record) in envelope.into_records(&context) {
            if let Err(error) = state.writer.push(&record).await {
                tracing::error!(%error, "replay recording failed");
                state.failure = Some(error);
                state.shutdown.cancel();
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            if let Some(id) = id {
                responses.push(json!({"jsonrpc":"2.0","id":id,"result":null}));
            }
        }
        if responses.is_empty() {
            return StatusCode::NO_CONTENT.into_response();
        }
        if is_batch {
            Json(responses).into_response()
        } else {
            Json(responses.pop()).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use base_clock::GlobalClock;
    use base_replay_driver::{BuilderIngressInjector, Injector};
    use base_replay_store::{ReplayStore, ReplayWriter};
    use futures::TryStreamExt;
    use reth_tasks::{RuntimeBuilder, RuntimeConfig, TokioConfig};
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, task::JoinSet, time::timeout};
    use tokio_util::sync::CancellationToken;

    use super::Recorder;
    use crate::{CaptureContext, RpcEnvelope};

    #[tokio::test]
    async fn records_single_and_batch_verbatim_when_shutdown_flushes() {
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
        let params = r#"[ { "n" : 1.2300e+02, "s": "\u0061" } ]"#;
        let call = format!(r#"{{"jsonrpc":"2.0","id":7,"method":"future","params":{params}}}"#);
        let single = json!({"jsonrpc":"2.0","id":7,"result":null});
        for (body, expected) in
            [(call.clone(), single.clone()), (format!("[{call},{call}]"), json!([single, single]))]
        {
            let response = client.post(&url).body(body).send().await.unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(
                serde_json::from_str::<Value>(&response.text().await.unwrap()).unwrap(),
                expected
            );
        }
        for body in ["[]", "not json", "{\"jsonrpc\":\"2.0\",\"method\":\"bad\",\"params\":[\n1]}"]
        {
            assert_eq!(client.post(&url).body(body).send().await.unwrap().status(), 400);
        }
        let oversized = "x".repeat(Recorder::MAX_BODY_BYTES + 1);
        let response = client.post(&url).body(oversized).send().await.unwrap();
        assert_eq!(response.status(), 413);
        cancel.cancel();
        timeout(Duration::from_secs(10), tasks.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .unwrap();

        let end = u64::try_from(GlobalClock::get().now_unix_millis().unwrap()).unwrap();
        let records: Vec<_> = store.read("devnet", start..=end).try_collect().await.unwrap();
        assert_eq!(
            records.iter().map(|r| r.batch_id).collect::<Vec<_>>(),
            [None, Some(0), Some(0)]
        );
        assert!(records.iter().all(|r| r.params.get().as_bytes() == params.as_bytes()));
        assert!(records.iter().all(|r| r.source.starts_with("127.0.0.1:")));
    }

    #[tokio::test]
    async fn returns_error_when_final_minute_cannot_be_published() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blocked");
        std::fs::write(&root, b"not a directory").unwrap();
        let mut writer = ReplayWriter::new(ReplayStore::Local(root), "devnet".into()).unwrap();
        let record = serde_json::from_str(r#"{"stream":"builder_ingress","received_at_ms":0,"method":"call","params":[],"source":"x"}"#).unwrap();
        writer.push(&record).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();

        assert!(Recorder::new(writer, cancel).serve(listener).await.is_err());
    }

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
        let context = CaptureContext {
            received_at_ms: 0,
            source: "127.0.0.1:1".parse().unwrap(),
            batch_id: 0,
        };
        let (_, record) =
            RpcEnvelope::parse(fixture).unwrap().into_records(&context).pop().unwrap();
        let params = record.params.get().to_owned();
        let hash = record.tx_hash;
        let start = u64::try_from(GlobalClock::get().now_unix_millis().unwrap()).unwrap();
        let injector = BuilderIngressInjector::new(url.parse().unwrap(), &runtime).unwrap();

        injector.inject(record).await.unwrap();
        drop(injector);
        cancel.cancel();
        timeout(Duration::from_secs(10), tasks.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .unwrap();

        let end = u64::try_from(GlobalClock::get().now_unix_millis().unwrap()).unwrap();
        let records: Vec<_> = store.read("devnet", start..=end).try_collect().await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].params.get(), params);
        assert_eq!(records[0].tx_hash, hash);
    }
}
