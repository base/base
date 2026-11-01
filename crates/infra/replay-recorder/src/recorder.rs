use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Extension, Router,
    body::to_bytes,
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use base_replay_store::{ReplayWriter, StoreError};
use hyper::server::conn::http1;
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use tokio::{net::TcpListener, task::JoinSet, time::timeout};
use tokio_util::sync::CancellationToken;

use crate::ArrivalQueue;

/// HTTP capture service owning both connections and the final writer flush.
#[derive(Debug)]
pub struct Recorder {
    writer: ReplayWriter,
    shutdown: CancellationToken,
}

impl Recorder {
    /// Maximum collected HTTP body, 8 `MiB`.
    pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
    /// Maximum connection tasks, including clients not reading responses.
    pub const MAX_CONNECTIONS: usize = 8;
    /// Grace period before remaining connections are aborted and joined.
    pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

    /// Create one capture session owning its writer and shutdown token.
    pub const fn new(writer: ReplayWriter, shutdown: CancellationToken) -> Self {
        Self { writer, shutdown }
    }

    /// Stop admission, bound connection draining, then always await writer close.
    pub async fn serve(self, listener: TcpListener) -> Result<(), StoreError> {
        let (queue, jobs) = ArrivalQueue::channel();
        let queue = Arc::new(queue);
        let worker = tokio::spawn(ArrivalQueue::write(self.writer, jobs, self.shutdown.clone()));
        let app = Router::new().route("/", post(Self::record)).with_state(Arc::clone(&queue));
        let mut connections = JoinSet::new();
        tracing::info!(listen = ?listener.local_addr(), "replay recorder listening");
        let serving = loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break Ok(()),
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(error) = result { break Err(std::io::Error::other(error)); }
                }
                accepted = listener.accept() => {
                    let (stream, peer) = match accepted { Ok(pair) => pair, Err(error) => break Err(error) };
                    if connections.len() >= Self::MAX_CONNECTIONS { continue; }
                    let app = app.clone().layer(Extension(ConnectInfo(peer)));
                    let shutdown = self.shutdown.clone();
                    connections.spawn(async move {
                        let connection = http1::Builder::new().serve_connection(TokioIo::new(stream), TowerToHyperService::new(app));
                        tokio::pin!(connection);
                        let result = tokio::select! {
                            result = &mut connection => result,
                            _ = shutdown.cancelled() => {
                                connection.as_mut().graceful_shutdown();
                                connection.await
                            }
                        };
                        if let Err(error) = result { tracing::debug!(%error, "recorder connection closed"); }
                    });
                }
            }
        };
        self.shutdown.cancel();
        if timeout(Self::DRAIN_TIMEOUT, async { while connections.join_next().await.is_some() {} })
            .await
            .is_err()
        {
            tracing::warn!("recorder connection drain timed out");
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        }
        drop(app);
        drop(queue);
        worker.await??;
        Ok(serving?)
    }

    /// Reject excess admission before polling or buffering the request body.
    pub async fn record(
        State(queue): State<Arc<ArrivalQueue>>,
        ConnectInfo(source): ConnectInfo<SocketAddr>,
        request: Request,
    ) -> Response {
        let (body, response) = match queue.admit(source) {
            Ok(slot) => slot,
            Err(status) => return status.into_response(),
        };
        let bytes = match timeout(
            Duration::from_secs(10),
            to_bytes(request.into_body(), Self::MAX_BODY_BYTES),
        )
        .await
        {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
            Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
        };
        if body.send(bytes).is_err() {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        response.await.unwrap_or_else(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use base_clock::GlobalClock;
    use base_replay_store::{ReplayStore, ReplayWriter};
    use futures::TryStreamExt;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpSocket},
        task::JoinSet,
        time::timeout,
    };
    use tokio_util::sync::CancellationToken;

    use super::Recorder;

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
    async fn closes_writer_when_client_stops_reading_large_response() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReplayStore::Local(dir.path().into());
        let writer = ReplayWriter::new(store.clone(), "devnet".into()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let mut tasks = JoinSet::new();
        tasks.spawn(Recorder::new(writer, cancel.clone()).serve(listener));
        let socket = TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(1024).unwrap();
        let mut stalled = socket.connect(address).await.unwrap();
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":"{}","method":"large"}}"#,
            "x".repeat(2 * 1024 * 1024)
        );
        let headers = format!(
            "POST / HTTP/1.1\r\nHost: local\r\nExpect: 100-continue\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        stalled.write_all(headers.as_bytes()).await.unwrap();
        let mut interim = [0; 25];
        stalled.read_exact(&mut interim).await.unwrap();
        stalled.write_all(body.as_bytes()).await.unwrap();
        let client = reqwest::Client::builder().timeout(Duration::from_secs(3)).build().unwrap();
        assert_eq!(
            client
                .post(format!("http://{address}"))
                .body(r#"{"jsonrpc":"2.0","id":2,"method":"probe"}"#)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        cancel.cancel();
        timeout(Duration::from_secs(3), tasks.join_next())
            .await
            .expect("bounded shutdown")
            .unwrap()
            .unwrap()
            .unwrap();
        let end = u64::try_from(GlobalClock::get().now_unix_millis().unwrap()).unwrap();
        let records: Vec<_> = store.read("devnet", end - 60_000..=end).try_collect().await.unwrap();
        assert_eq!(records.len(), 2);
    }
}
