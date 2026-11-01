use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{
    body::Bytes,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use base_clock::GlobalClock;
use base_replay_store::{ReplayWriter, StoreError};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{CaptureContext, RpcEnvelope};

/// Body delivery and eventual response for one admitted request.
pub type Admission = (oneshot::Sender<Bytes>, oneshot::Receiver<Response>);

/// Arrival-ordered slot, reserved before any body bytes are collected.
#[derive(Debug)]
pub struct CaptureJob {
    /// Arrival timestamp, remote address, and sequence number.
    pub context: CaptureContext,
    /// Bounded-body producer; dropping it cancels this slot.
    pub body: oneshot::Receiver<Bytes>,
    /// Response destination; disconnection does not cancel accepted writes.
    pub response: oneshot::Sender<Response>,
    /// Admission remains occupied until processing finishes.
    pub permit: OwnedSemaphorePermit,
}

/// Bounded admission with a single timestamp-ordered writer.
#[derive(Debug)]
pub struct ArrivalQueue {
    sender: mpsc::Sender<CaptureJob>,
    slots: Arc<Semaphore>,
    sequence: Mutex<u64>,
}

impl ArrivalQueue {
    /// Maximum bodies admitted or waiting on storage at once.
    pub const CAPACITY: usize = 4;
    /// JSON-RPC error for an invalid individual member.
    pub const INVALID: &'static str =
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid request"}}"#;
    /// JSON-RPC error for malformed JSON text.
    pub const PARSE_ERROR: &'static str =
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}"#;

    /// Create an admission boundary and its ordered consumer.
    pub fn channel() -> (Self, mpsc::Receiver<CaptureJob>) {
        let (sender, receiver) = mpsc::channel(Self::CAPACITY);
        (
            Self {
                sender,
                slots: Arc::new(Semaphore::new(Self::CAPACITY)),
                sequence: Mutex::new(0),
            },
            receiver,
        )
    }

    /// Reserve and timestamp synchronously; return 503 without reading a body when full.
    pub fn admit(&self, source: SocketAddr) -> Result<Admission, StatusCode> {
        let permit = Arc::clone(&self.slots)
            .try_acquire_owned()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let mut sequence = self.sequence.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let received_at_ms = GlobalClock::get()
            .now_unix_millis()
            .ok()
            .and_then(|ms| u64::try_from(ms).ok())
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
        let context = CaptureContext { received_at_ms, source, batch_id: *sequence };
        let (body, received) = oneshot::channel();
        let (response, reply) = oneshot::channel();
        self.sender
            .try_send(CaptureJob { context, body: received, response, permit })
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        *sequence += 1;
        Ok((body, reply))
    }

    /// Process FIFO slots, then always close the writer and propagate publication errors.
    pub async fn write(
        mut writer: ReplayWriter,
        mut jobs: mpsc::Receiver<CaptureJob>,
        shutdown: CancellationToken,
    ) -> Result<(), StoreError> {
        let mut failure = None;
        while let Some(job) = jobs.recv().await {
            let Ok(body) = job.body.await else { continue };
            let envelope = match RpcEnvelope::parse(&body) {
                Ok(envelope) => envelope,
                Err(error) => {
                    let body = if error.is_syntax() || error.is_eof() {
                        Self::PARSE_ERROR
                    } else {
                        Self::INVALID
                    };
                    let _ = job.response.send((StatusCode::BAD_REQUEST, body).into_response());
                    continue;
                }
            };
            let is_batch = matches!(&envelope, RpcEnvelope::Batch(_));
            let mut responses = Vec::new();
            for entry in envelope.into_records(&job.context) {
                match entry {
                    Ok((id, record)) => {
                        if let Err(error) = writer.push(&record).await {
                            tracing::error!(%error, "replay recording failed");
                            failure = Some(error);
                            shutdown.cancel();
                            break;
                        }
                        if let Some(id) = id {
                            responses.push(format!(
                                r#"{{"jsonrpc":"2.0","id":{},"result":null}}"#,
                                id.0.get()
                            ));
                        }
                    }
                    Err(_) => responses.push(Self::INVALID.to_owned()),
                }
            }
            let response = if failure.is_some() {
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            } else if responses.is_empty() {
                StatusCode::NO_CONTENT.into_response()
            } else {
                let body = if is_batch {
                    format!("[{}]", responses.join(","))
                } else {
                    responses.pop().unwrap_or_default()
                };
                ([(header::CONTENT_TYPE, "application/json")], body).into_response()
            };
            let _ = job.response.send(response);
            if failure.is_some() {
                break;
            }
        }
        writer.close().await?;
        failure.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use axum::body::{Bytes, to_bytes};
    use base_replay_store::{ReplayStore, ReplayWriter};
    use futures::TryStreamExt;
    use tokio::sync::{Semaphore, mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    use super::{ArrivalQueue, CaptureContext, CaptureJob, StatusCode};

    #[test]
    fn rejects_excess_admission_without_collecting_bodies() {
        let (queue, _jobs) = ArrivalQueue::channel();
        let held: Vec<_> = (0..ArrivalQueue::CAPACITY)
            .map(|_| queue.admit("127.0.0.1:1".parse().unwrap()).unwrap())
            .collect();
        assert_eq!(
            queue.admit("127.0.0.1:1".parse().unwrap()).unwrap_err(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        drop(held);
    }

    #[tokio::test]
    async fn distinguishes_parse_errors_when_envelopes_are_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let writer =
            ReplayWriter::new(ReplayStore::Local(dir.path().into()), "devnet".into()).unwrap();
        let (queue, jobs) = ArrivalQueue::channel();
        let task = tokio::spawn(ArrivalQueue::write(writer, jobs, CancellationToken::new()));
        for (input, code) in [("{", -32700), ("[]", -32600), ("{}", -32600), ("1", -32600)] {
            let (body, reply) = queue.admit("127.0.0.1:1".parse().unwrap()).unwrap();
            body.send(Bytes::from_static(input.as_bytes())).unwrap();
            let response = reply.await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
            let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(error["error"]["code"], code, "{input}");
            assert_eq!(error["id"], serde_json::Value::Null);
        }
        drop(queue);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn preserves_arrival_timestamps_when_later_body_finishes_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReplayStore::Local(dir.path().into());
        let writer = ReplayWriter::new(store.clone(), "devnet".into()).unwrap();
        let (sender, jobs) = mpsc::channel(2);
        let mut bodies = Vec::new();
        let mut replies = Vec::new();
        for ms in [100, 200] {
            let (body, received) = oneshot::channel();
            let (response, reply) = oneshot::channel();
            sender
                .send(CaptureJob {
                    context: CaptureContext {
                        received_at_ms: ms,
                        source: "127.0.0.1:1".parse().unwrap(),
                        batch_id: ms,
                    },
                    body: received,
                    response,
                    permit: std::sync::Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap(),
                })
                .await
                .unwrap();
            bodies.push(body);
            replies.push(reply);
        }
        let task = tokio::spawn(ArrivalQueue::write(writer, jobs, CancellationToken::new()));
        let input =
            Bytes::from_static(br#"[{"jsonrpc":"2.0","id":18446744073709551617,"method":"ok"},1]"#);
        bodies.pop().unwrap().send(input.clone()).unwrap();
        bodies.pop().unwrap().send(input).unwrap();
        drop(sender);
        task.await.unwrap().unwrap();
        for reply in replies {
            let body = to_bytes(reply.await.unwrap().into_body(), 4096).await.unwrap();
            assert_eq!(body, Bytes::from_static(br#"[{"jsonrpc":"2.0","id":18446744073709551617,"result":null},{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid request"}}]"#));
        }
        let records: Vec<_> = store.read("devnet", 0..=200).try_collect().await.unwrap();
        assert_eq!(records.iter().map(|r| r.received_at_ms).collect::<Vec<_>>(), [100, 200]);
    }
}
