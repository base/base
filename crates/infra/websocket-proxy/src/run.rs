use std::{io::Write, sync::Arc, time::Duration};

use axum::extract::ws::Message;
use metrics_exporter_prometheus::PrometheusBuilder;
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::broadcast,
    time::interval,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, trace};

use crate::{
    InMemoryRateLimit, Metrics, RateLimit, Registry, Server, WebsocketProxyConfig,
    WebsocketSubscriber,
};

/// Wrapper for the websocket proxy entry point.
#[derive(Debug)]
pub struct WebsocketProxyRunner;

impl WebsocketProxyRunner {
    /// Run the websocket proxy with the given configuration.
    ///
    /// Sets up metrics, upstream subscribers, client ping tasks, the server, and
    /// graceful shutdown handling. Returns when the service terminates.
    pub async fn run(config: WebsocketProxyConfig) -> anyhow::Result<()> {
        if let Some(metrics_config) = &config.metrics_config {
            let metrics_addr =
                std::net::SocketAddr::from(([0, 0, 0, 0], metrics_config.port));

            info!(address = %metrics_addr, "starting metrics server");

            let mut builder = PrometheusBuilder::new().with_http_listener(metrics_addr);

            for (key, value) in &metrics_config.global_labels {
                builder = builder.add_global_label(key, value);
            }

            builder.install().expect("failed to setup Prometheus endpoint");
        }

        if config.upstream_ws.is_empty() {
            anyhow::bail!("no upstream URIs provided");
        }

        info!(uris = ?config.upstream_ws, "using upstream URIs");

        let metrics = Arc::new(Metrics::default());
        let metrics_clone = Arc::clone(&metrics);
        let enable_compression = config.enable_compression;

        let (send, _rec) = broadcast::channel(config.message_buffer_size);
        let sender = send.clone();

        let listener = move |data: String| {
            trace!(data = data, "received data");
            // Subtract one from receiver count, as we keep one receiver open at all times (see _rec)
            // to avoid the channel being closed. This is not an active client connection.
            metrics_clone
                .active_connections
                .set((send.receiver_count() - 1) as f64);

            let message_data = if enable_compression {
                let data_bytes = data.as_bytes();
                let mut compressed_data_bytes = Vec::new();
                {
                    let mut compressor =
                        brotli::CompressorWriter::new(&mut compressed_data_bytes, 4096, 5, 22);
                    compressor.write_all(data_bytes).unwrap();
                }
                compressed_data_bytes
            } else {
                data.into_bytes()
            };

            match send.send(message_data.into()) {
                Ok(_) => {
                    metrics_clone.broadcast_queue_size.set(send.len() as f64);
                }
                Err(e) => error!(error = %e, "failed to send data"),
            }
        };

        let token = CancellationToken::new();
        let mut subscriber_tasks = Vec::new();

        for (index, uri) in config.upstream_ws.iter().enumerate() {
            let uri_clone = uri.clone();
            let listener_clone = listener.clone();
            let token_clone = token.clone();
            let metrics_clone = Arc::clone(&metrics);

            let options = config.subscriber_options.clone();

            let mut subscriber =
                WebsocketSubscriber::new(uri_clone.clone(), listener_clone, metrics_clone, options);

            let task = tokio::spawn(async move {
                info!(index = index, uri = %uri_clone, "starting subscriber");
                subscriber.run(token_clone).await;
            });

            subscriber_tasks.push(task);
        }

        let ping_task = if config.client_ping.enabled {
            let ping_sender = sender.clone();
            let ping_token = token.clone();
            let ping_interval_ms = config.client_ping.interval_secs * 1000;
            let ping_metrics = Arc::clone(&metrics);

            tokio::spawn(async move {
                let mut interval = interval(Duration::from_millis(ping_interval_ms));
                info!(interval_ms = ping_interval_ms, "starting ping sender");

                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            match ping_sender.send(Message::Ping(vec![].into())) {
                                Ok(_) => {
                                    trace!("sent ping to all clients");
                                    ping_metrics
                                        .broadcast_queue_size
                                        .set(ping_sender.len() as f64);
                                }
                                Err(e) => error!(error = %e, "failed to send ping"),
                            }
                        }
                        _ = ping_token.cancelled() => {
                            info!("ping sender shutting down");
                            break;
                        }
                    }
                }
            })
        } else {
            tokio::spawn(std::future::pending())
        };

        let registry = Registry::new(
            sender,
            Arc::clone(&metrics),
            config.enable_compression,
            config.client_ping.enabled,
            config.client_ping.pong_timeout_ms,
            Duration::from_millis(config.client_send_timeout_ms),
        );

        let rate_limiter: Arc<dyn RateLimit> = Arc::new(InMemoryRateLimit::new(
            config.instance_connection_limit,
            config.per_ip_connection_limit,
        ));

        let server = Server::new(
            config.listen_addr,
            registry.clone(),
            metrics,
            rate_limiter,
            config.authentication,
            config.ip_addr_http_header,
            config.public_access_enabled,
        );
        let server_task = server.listen(token.clone());

        let mut interrupt = signal(SignalKind::interrupt()).unwrap();
        let mut terminate = signal(SignalKind::terminate()).unwrap();

        tokio::select! {
            _ = futures::future::join_all(subscriber_tasks) => {
                info!("all subscriber tasks terminated");
                token.cancel();
            },
            _ = server_task => {
                info!("server task terminated");
                token.cancel();
            },
            _ = ping_task => {
                info!("ping task terminated");
                token.cancel();
            },
            _ = interrupt.recv() => {
                info!("process interrupted, shutting down");
                token.cancel();
            }
            _ = terminate.recv() => {
                info!("process terminated, shutting down");
                token.cancel();
            }
        }

        Ok(())
    }
}
