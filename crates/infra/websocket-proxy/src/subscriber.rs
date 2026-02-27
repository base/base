use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::http::Uri;
use backoff::{ExponentialBackoff, backoff::Backoff};
use futures::{SinkExt, StreamExt};
use tokio::{select, sync::oneshot};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error, Error::ConnectionClosed, Message},
};
use tokio_util::{bytes, sync::CancellationToken};
use tracing::{error, info, trace, warn};

use crate::metrics::Metrics;

/// Configuration options for a [`WebsocketSubscriber`].
#[derive(Debug, Clone)]
pub struct SubscriberOptions {
    /// Maximum duration between reconnection attempts.
    pub max_backoff_interval: Duration,
    /// Initial duration between reconnection attempts before exponential increase.
    pub backoff_initial_interval: Duration,
    /// Interval at which ping frames are sent to the upstream server.
    pub ping_interval: Duration,
    /// Maximum time to wait for a pong response before considering the connection dead.
    pub pong_timeout: Duration,
    /// Grace period after initial connection before enforcing pong deadlines.
    pub initial_grace_period: Duration,
}

impl SubscriberOptions {
    /// Sets the maximum backoff interval.
    pub const fn with_max_backoff_interval(mut self, max_backoff_interval: Duration) -> Self {
        self.max_backoff_interval = max_backoff_interval;
        self
    }

    /// Sets the ping interval.
    pub const fn with_ping_interval(mut self, ping_interval: Duration) -> Self {
        self.ping_interval = ping_interval;
        self
    }

    /// Sets the pong timeout.
    pub const fn with_pong_timeout(mut self, pong_timeout: Duration) -> Self {
        self.pong_timeout = pong_timeout;
        self
    }

    /// Sets the initial backoff interval.
    pub const fn with_backoff_initial_interval(
        mut self,
        backoff_initial_interval: Duration,
    ) -> Self {
        self.backoff_initial_interval = backoff_initial_interval;
        self
    }

    /// Sets the initial grace period.
    pub const fn with_initial_grace_period(mut self, initial_grace_period: Duration) -> Self {
        self.initial_grace_period = initial_grace_period;
        self
    }
}

impl Default for SubscriberOptions {
    fn default() -> Self {
        Self {
            max_backoff_interval: Duration::from_secs(5),
            backoff_initial_interval: Duration::from_millis(500),
            ping_interval: Duration::from_secs(1),
            pong_timeout: Duration::from_secs(2),
            initial_grace_period: Duration::from_secs(5),
        }
    }
}

/// Builds a reconnect URI by appending `?block_number=N&flashblock_index=M` to the base.
///
/// Any existing query string on the base URI is replaced. Returns the base URI unchanged
/// if no resume position is provided. Always ensures at least a `/` path so the resulting
/// URI produces a valid HTTP request line (`GET /... HTTP/1.1`).
fn uri_with_resume(base: &Uri, pos: Option<(u64, u64)>) -> Uri {
    let (bn, fi) = match pos {
        Some(p) => p,
        None => return base.clone(),
    };

    let scheme = base.scheme_str().unwrap_or("ws");
    let authority = base.authority().map(|a| a.as_str()).unwrap_or("");
    let path = {
        let p = base.path();
        if p.is_empty() { "/" } else { p }
    };

    format!("{scheme}://{authority}{path}?block_number={bn}&flashblock_index={fi}")
        .parse()
        .unwrap_or_else(|_| base.clone())
}

/// Maintains a persistent websocket connection to an upstream server, automatically
/// reconnecting with exponential backoff and monitoring liveness via ping/pong.
///
/// Tracks the last received `(block_number, flashblock_index)` position and appends
/// it as query parameters when reconnecting, so the upstream can resume sending from
/// the correct position.
pub struct WebsocketSubscriber<F>
where
    F: Fn(String) + Send + Sync + 'static,
{
    uri: Uri,
    handler: F,
    backoff: ExponentialBackoff,
    metrics: Arc<Metrics>,
    options: SubscriberOptions,
    last_position: Option<(u64, u64)>,
}

impl<F> std::fmt::Debug for WebsocketSubscriber<F>
where
    F: Fn(String) + Send + Sync + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebsocketSubscriber")
            .field("uri", &self.uri)
            .field("options", &self.options)
            .field("last_position", &self.last_position)
            .finish_non_exhaustive()
    }
}

impl<F> WebsocketSubscriber<F>
where
    F: Fn(String) + Send + Sync + 'static,
{
    /// Creates a new subscriber targeting the given URI with the provided message handler.
    pub fn new(uri: Uri, handler: F, metrics: Arc<Metrics>, options: SubscriberOptions) -> Self {
        let backoff = ExponentialBackoff {
            initial_interval: options.backoff_initial_interval,
            max_interval: options.max_backoff_interval,
            max_elapsed_time: None,
            ..Default::default()
        };

        Self { uri, handler, backoff, metrics, options, last_position: None }
    }

    /// Runs the subscriber loop, reconnecting on failure until the token is cancelled.
    pub async fn run(&mut self, token: CancellationToken) {
        info!(message = "starting upstream subscription", uri = self.uri.to_string());
        loop {
            select! {
                _ = token.cancelled() => {
                    info!(
                        message = "cancelled upstream subscription",
                        uri = self.uri.to_string()
                    );
                    return;
                }
                result = self.connect_and_listen() => {
                    match result {
                        Ok(()) => {
                            info!(
                                message = "upstream connection closed",
                                uri = self.uri.to_string()
                            );
                        }
                        Err(e) => {
                            error!(
                                message = "upstream websocket error",
                                uri = self.uri.to_string(),
                                error = e.to_string()
                            );
                            self.metrics.upstream_errors.increment(1);

                            if let Some(duration) = self.backoff.next_backoff() {
                                warn!(
                                    message = "reconnecting",
                                    uri = self.uri.to_string(),
                                    seconds = duration.as_secs()
                                );
                                select! {
                                    _ = token.cancelled() => {
                                        info!(
                                            message = "cancelled subscriber during backoff",
                                            uri = self.uri.to_string()
                                        );
                                        return
                                    }
                                    _ = tokio::time::sleep(duration) => {}
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    async fn connect_and_listen(&mut self) -> Result<(), Error> {
        let connect_uri = uri_with_resume(&self.uri, self.last_position);
        info!(message = "connecting to websocket", uri = connect_uri.to_string());

        self.metrics.upstream_connection_attempts.increment(1);

        let (ws_stream, _) = match connect_async(&connect_uri).await {
            Ok(connection) => {
                self.metrics.upstream_connection_successes.increment(1);
                connection
            }
            Err(e) => {
                self.metrics.upstream_connection_failures.increment(1);
                return Err(e);
            }
        };

        info!(message = "websocket connection established", uri = connect_uri.to_string());

        self.metrics.upstream_connections.increment(1);
        self.backoff.reset();

        let (mut write, mut read) = ws_stream.split();

        let (ping_error_tx, mut ping_error_rx) = oneshot::channel();
        let options = self.options.clone();
        let metrics = Arc::clone(&self.metrics);
        let mut pong_deadline = Instant::now() + options.initial_grace_period;

        let ping_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(options.ping_interval);
            loop {
                interval.tick().await;
                metrics.ping_attempts.increment(1);
                if let Err(e) = write.send(Message::Ping(bytes::Bytes::new())).await {
                    error!(message = "failed to send ping to upstream", error = e.to_string());
                    metrics.ping_failures.increment(1);
                    let _ = ping_error_tx.send(e);
                    break;
                }
                metrics.ping_sent.increment(1);
            }
        });

        let mut deadline_check = tokio::time::interval(Duration::from_millis(50));

        let result = loop {
            select! {
                _ = deadline_check.tick() => {
                    if Instant::now() >= pong_deadline {
                        error!(
                            message = "pong timeout from upstream",
                            uri = self.uri.to_string()
                        );
                        break Err(ConnectionClosed);
                    }
                }
                Ok(ping_err) = &mut ping_error_rx => {
                    break Err(ping_err);
                }
                message = read.next() => {
                    let Some(msg) = message else {
                        break Ok(());
                    };
                    if let Err(e) = self.handle_message(msg, &mut pong_deadline, options.pong_timeout) {
                        break Err(e);
                    }
                }
            }
        };

        ping_task.abort();
        self.metrics.upstream_connections.decrement(1);
        result
    }

    fn handle_message(
        &mut self,
        message: Result<Message, Error>,
        pong_deadline: &mut Instant,
        pong_timeout: Duration,
    ) -> Result<(), Error> {
        let msg = match message {
            Ok(msg) => msg,
            Err(e) => {
                error!(
                    message = "error receiving message",
                    uri = self.uri.to_string(),
                    error = e.to_string()
                );
                return Err(e);
            }
        };

        match msg {
            Message::Text(text) => {
                trace!(
                    message = "received text message",
                    uri = self.uri.to_string(),
                    payload = text.as_str()
                );
                self.metrics.message_received_from_upstream(self.uri.to_string().as_str());

                // Update last position for reconnect URI.
                if let Some(pos) = extract_flashblock_position(text.as_str()) {
                    self.last_position = Some(pos);
                }

                (self.handler)(text.to_string());
            }
            Message::Binary(data) => {
                warn!(
                    message = "received binary message, unsupported",
                    uri = self.uri.to_string(),
                    size = data.len()
                );
            }
            Message::Pong(_) => {
                trace!(message = "received pong from upstream", uri = self.uri.to_string());
                *pong_deadline = Instant::now() + pong_timeout;
            }
            Message::Close(_) => {
                info!(message = "received close frame from upstream", uri = self.uri.to_string());
                return Err(ConnectionClosed);
            }
            _ => {}
        }

        Ok(())
    }
}

/// Extracts `(block_number, flashblock_index)` from a flashblock JSON string.
fn extract_flashblock_position(json: &str) -> Option<(u64, u64)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let index = v.get("index")?.as_u64()?;
    let block_number = v.get("metadata")?.get("block_number")?.as_u64()?;
    Some((block_number, index))
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{Arc, Mutex},
    };

    use axum::http::Uri;
    use futures::SinkExt;
    use tokio::{
        net::{TcpListener, TcpStream},
        sync::broadcast,
        time::{Duration, sleep, timeout},
    };
    use tokio_tungstenite::{
        accept_async,
        tungstenite::{Message, handshake::server::{Request, Response}},
    };

    use super::*;
    use crate::metrics::Metrics;

    struct MockServer {
        addr: SocketAddr,
        message_sender: broadcast::Sender<String>,
        shutdown: CancellationToken,
    }

    impl MockServer {
        async fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (tx, _) = broadcast::channel::<String>(100);
            let shutdown = CancellationToken::new();
            let shutdown_clone = shutdown.clone();
            let tx_clone = tx.clone();

            tokio::spawn(async move {
                loop {
                    select! {
                        _ = shutdown_clone.cancelled() => {
                            break;
                        }
                        accept_result = listener.accept() => {
                            match accept_result {
                                Ok((stream, _)) => {
                                    let tx = tx_clone.clone();
                                    let shutdown = shutdown_clone.clone();
                                    tokio::spawn(async move {
                                        Self::handle_connection(stream, tx, shutdown).await;
                                    });
                                }
                                Err(e) => {
                                    eprintln!("Failed to accept: {e}");
                                    break;
                                }
                            }
                        }
                    }
                }
            });

            Self { addr, message_sender: tx, shutdown }
        }

        async fn handle_connection(
            stream: TcpStream,
            tx: broadcast::Sender<String>,
            shutdown: CancellationToken,
        ) {
            let ws_stream = match accept_async(stream).await {
                Ok(ws_stream) => ws_stream,
                Err(e) => {
                    eprintln!("Failed to accept websocket: {e}");
                    return;
                }
            };

            let (mut ws_sender, _) = ws_stream.split();

            let mut rx = tx.subscribe();

            loop {
                select! {
                    _ = shutdown.cancelled() => {
                        break;
                    }
                    msg = rx.recv() => {
                        match msg {
                            Ok(text) => {
                                if let Err(e) = ws_sender.send(Message::Text(text.into())).await {
                                    eprintln!("Error sending message: {e}");
                                    break;
                                }
                            }
                            Err(_) => {
                                break;
                            }
                        }
                    }
                }
            }
        }

        async fn send_message(
            &self,
            msg: &str,
        ) -> Result<usize, broadcast::error::SendError<String>> {
            self.message_sender.send(msg.to_string())
        }

        async fn shutdown(self) {
            self.shutdown.cancel();
        }

        fn uri(&self) -> Uri {
            format!("ws://{}", self.addr).parse().expect("Failed to parse URI")
        }
    }

    #[test]
    fn uri_with_resume_appends_params() {
        let base: Uri = "ws://127.0.0.1:9000".parse().unwrap();
        let result = uri_with_resume(&base, Some((42, 3)));
        // Path defaults to "/" so the HTTP request line is valid.
        assert_eq!(result.to_string(), "ws://127.0.0.1:9000/?block_number=42&flashblock_index=3");
    }

    #[test]
    fn uri_with_resume_preserves_path() {
        let base: Uri = "ws://127.0.0.1:9000/upstream".parse().unwrap();
        let result = uri_with_resume(&base, Some((42, 3)));
        assert_eq!(
            result.to_string(),
            "ws://127.0.0.1:9000/upstream?block_number=42&flashblock_index=3"
        );
    }

    #[test]
    fn uri_with_resume_replaces_existing_query() {
        let base: Uri = "ws://127.0.0.1:9000/?foo=bar".parse().unwrap();
        let result = uri_with_resume(&base, Some((10, 5)));
        assert_eq!(result.to_string(), "ws://127.0.0.1:9000/?block_number=10&flashblock_index=5");
    }

    #[test]
    fn uri_with_resume_none_returns_base() {
        let base: Uri = "ws://127.0.0.1:9000".parse().unwrap();
        let result = uri_with_resume(&base, None);
        assert_eq!(result, base);
    }

    #[test]
    fn extract_flashblock_position_valid() {
        let json = r#"{"index":2,"metadata":{"block_number":99}}"#;
        assert_eq!(extract_flashblock_position(json), Some((99, 2)));
    }

    #[test]
    fn extract_flashblock_position_missing() {
        assert_eq!(extract_flashblock_position(r#"{"index":1}"#), None);
        assert_eq!(extract_flashblock_position("not json"), None);
    }

    #[tokio::test]
    async fn test_ping_pong_reconnection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let uri: Uri = format!("ws://{addr}").parse().unwrap();

        let shutdown = CancellationToken::new();
        let shutdown_server = shutdown.clone();

        let connection_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let connection_count_server = Arc::clone(&connection_count);

        tokio::spawn(async move {
            loop {
                select! {
                    _ = shutdown_server.cancelled() => break,
                    accept_result = listener.accept() => {
                        if let Ok((stream, _)) = accept_result {
                            connection_count_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let shutdown_inner = shutdown_server.clone();
                            tokio::spawn(async move {
                                let _ws_stream = match accept_async(stream).await {
                                    Ok(ws) => ws,
                                    Err(_) => return,
                                };


                                // Become completely unresponsive - don't read any messages
                                select! {
                                    _ = shutdown_inner.cancelled() => ()
                                }
                            });
                        }
                    }
                }
            }
        });

        let listener_fn = move |_data: String| {
            // Handler for received messages - not needed for this test
        };

        let options = SubscriberOptions::default()
            .with_backoff_initial_interval(Duration::from_millis(100))
            .with_ping_interval(Duration::from_millis(100))
            .with_pong_timeout(Duration::from_millis(200))
            .with_initial_grace_period(Duration::from_millis(50));

        let mut subscriber =
            WebsocketSubscriber::new(uri, listener_fn, Arc::new(Metrics::default()), options);

        let subscriber_task = {
            let token_clone = shutdown.clone();
            tokio::spawn(async move {
                subscriber.run(token_clone).await;
            })
        };

        // This needs to take into account the poll interval, pong deadline and the backoff interval.
        sleep(Duration::from_secs(1)).await;

        let connections = connection_count.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            connections >= 2,
            "Expected at least 2 connection attempts due to ping timeout, got {connections}"
        );

        shutdown.cancel();
        let _ = timeout(Duration::from_secs(1), subscriber_task).await;
    }

    #[tokio::test]
    async fn test_multiple_subscribers_single_listener() {
        let server1 = MockServer::new().await;
        let server2 = MockServer::new().await;

        let received_messages = Arc::new(Mutex::new(Vec::new()));
        let received_clone = Arc::clone(&received_messages);

        let listener = move |data: String| {
            if let Ok(mut messages) = received_clone.lock() {
                messages.push(data);
            }
        };

        let metrics = Arc::new(Metrics::default());

        let token = CancellationToken::new();
        let token_clone1 = token.clone();
        let token_clone2 = token.clone();

        let uri1 = server1.uri();
        let listener_clone1 = listener.clone();
        let metrics_clone1 = Arc::clone(&metrics);

        let mut subscriber1 = WebsocketSubscriber::new(
            uri1.clone(),
            listener_clone1,
            metrics_clone1,
            SubscriberOptions::default(),
        );

        let uri2 = server2.uri();
        let listener_clone2 = listener.clone();
        let metrics_clone2 = Arc::clone(&metrics);

        let mut subscriber2 = WebsocketSubscriber::new(
            uri2.clone(),
            listener_clone2,
            metrics_clone2,
            SubscriberOptions::default(),
        );

        let task1 = tokio::spawn(async move {
            subscriber1.run(token_clone1).await;
        });

        let task2 = tokio::spawn(async move {
            subscriber2.run(token_clone2).await;
        });

        sleep(Duration::from_millis(500)).await;

        let _ = server1.send_message("Message from server 1").await;
        let _ = server2.send_message("Message from server 2").await;

        sleep(Duration::from_millis(500)).await;

        let _ = server1.send_message("Another message from server 1").await;
        let _ = server2.send_message("Another message from server 2").await;

        sleep(Duration::from_millis(500)).await;

        // Cancel the token to shut down subscribers
        token.cancel();
        let _ = timeout(Duration::from_secs(1), task1).await;
        let _ = timeout(Duration::from_secs(1), task2).await;

        server1.shutdown().await;
        server2.shutdown().await;

        let messages = match received_messages.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        assert_eq!(messages.len(), 4);

        assert!(messages.contains(&"Message from server 1".to_string()));
        assert!(messages.contains(&"Message from server 2".to_string()));
        assert!(messages.contains(&"Another message from server 1".to_string()));
        assert!(messages.contains(&"Another message from server 2".to_string()));

        assert!(!messages.is_empty());
    }

    #[tokio::test]
    async fn test_subscriber_tracks_position_for_reconnect() {
        // Mock server that captures the URI used to connect
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let uri: Uri = format!("ws://{addr}").parse().unwrap();

        let connect_uris = Arc::new(Mutex::new(Vec::<String>::new()));
        let connect_uris_server = Arc::clone(&connect_uris);

        let shutdown = CancellationToken::new();
        let shutdown_server = shutdown.clone();
        let (msg_tx, _) = broadcast::channel::<String>(16);
        let msg_tx_server = msg_tx.clone();

        tokio::spawn(async move {
            let mut connection_count = 0u32;
            loop {
                select! {
                    _ = shutdown_server.cancelled() => break,
                    accept_result = listener.accept() => {
                        if let Ok((stream, _)) = accept_result {
                            // Peek at the raw request to capture the URI
                            connection_count += 1;
                            let uris = Arc::clone(&connect_uris_server);
                            let tx = msg_tx_server.clone();
                            let shutdown_inner = shutdown_server.clone();
                            let count = connection_count;
                            tokio::spawn(async move {
                                let captured = Arc::new(Mutex::new(String::new()));
                                let captured_cb = Arc::clone(&captured);
                                let ws = tokio_tungstenite::accept_hdr_async(
                                    stream,
                                    move |req: &Request, resp: Response| {
                                        *captured_cb.lock().unwrap() =
                                            req.uri().to_string();
                                        Ok(resp)
                                    },
                                )
                                .await
                                .unwrap();
                                uris.lock().unwrap().push(captured.lock().unwrap().clone());

                                let (mut sender, _) = ws.split();
                                if count == 1 {
                                    // First connection: send one flashblock then close
                                    let payload = r#"{"index":5,"metadata":{"block_number":100}}"#;
                                    sender.send(Message::Text(payload.into())).await.unwrap();
                                }
                                // Then wait for shutdown
                                select! {
                                    _ = shutdown_inner.cancelled() => {}
                                }
                            });
                        }
                    }
                }
            }
        });

        let options = SubscriberOptions::default()
            .with_backoff_initial_interval(Duration::from_millis(100))
            .with_ping_interval(Duration::from_millis(500))
            .with_pong_timeout(Duration::from_millis(200))
            .with_initial_grace_period(Duration::from_millis(500));

        let received = Arc::new(Mutex::new(Vec::<String>::new()));
        let received_clone = Arc::clone(&received);
        let mut subscriber = WebsocketSubscriber::new(
            uri,
            move |data: String| {
                received_clone.lock().unwrap().push(data);
            },
            Arc::new(Metrics::default()),
            options,
        );

        let shutdown_clone = shutdown.clone();
        let task = tokio::spawn(async move {
            subscriber.run(shutdown_clone).await;
        });

        // Wait enough time for: connect, receive 1 msg, disconnect, backoff, reconnect
        sleep(Duration::from_millis(800)).await;

        shutdown.cancel();
        let _ = timeout(Duration::from_secs(2), task).await;

        let uris = connect_uris.lock().unwrap();
        // Second connection (if it happened) should include position
        if uris.len() >= 2 {
            let second_uri = &uris[1];
            assert!(
                second_uri.contains("block_number=100"),
                "Second reconnect URI should include block_number=100, got: {second_uri}"
            );
            assert!(
                second_uri.contains("flashblock_index=5"),
                "Second reconnect URI should include flashblock_index=5, got: {second_uri}"
            );
        }
    }
}
