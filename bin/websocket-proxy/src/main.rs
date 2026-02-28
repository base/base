use std::{
    io::Write,
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::Duration,
};

use axum::{extract::ws::Message, http::Uri};
use base_cli_utils::LogConfig;
use clap::Parser;
use dotenvy::dotenv;
use metrics_exporter_prometheus::PrometheusBuilder;
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::broadcast,
    time::interval,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, trace, warn};
use websocket_proxy::{
    Authentication, FlashblocksRingBuffer, InMemoryRateLimit, Metrics, RateLimit, Registry,
    Server, SubscriberOptions, WebsocketSubscriber,
};

base_cli_utils::define_log_args!("WEBSOCKET_PROXY");

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[arg(
        long,
        env,
        default_value = "0.0.0.0:8545",
        help = "The address and port to listen on for incoming connections"
    )]
    listen_addr: SocketAddr,

    #[arg(
        long,
        env,
        value_delimiter = ',',
        help = "WebSocket URI of the upstream server to connect to"
    )]
    upstream_ws: Vec<Uri>,

    #[arg(
        long,
        env,
        default_value = "20",
        help = "Number of messages to buffer for lagging clients"
    )]
    message_buffer_size: usize,

    #[arg(
        long,
        env,
        default_value = "100",
        help = "Maximum number of concurrently connected clients per instance"
    )]
    instance_connection_limit: usize,

    #[arg(
        long,
        env,
        default_value = "10",
        help = "Maximum number of concurrently connected clients per IP"
    )]
    per_ip_connection_limit: usize,
    #[arg(
        long,
        env,
        default_value = "false",
        help = "Enable brotli compression on messages to downstream clients"
    )]
    enable_compression: bool,

    #[arg(
        long,
        env,
        default_value = "X-Forwarded-For",
        help = "Header to use to determine the clients origin IP"
    )]
    ip_addr_http_header: String,

    #[command(flatten)]
    log: LogArgs,

    /// Enable Prometheus metrics
    #[arg(long, env, default_value = "true")]
    metrics: bool,

    /// API Keys, if not provided will be an unauthenticated endpoint, should be in the format <app1>:<apiKey1>,<app2>:<apiKey2>,..
    #[arg(long, env, value_delimiter = ',', help = "API keys to allow")]
    api_keys: Vec<String>,

    /// Address to run the metrics server on
    #[arg(long, env, default_value = "0.0.0.0:9000")]
    metrics_addr: SocketAddr,

    /// Tags to add to every metrics emitted, should be in the format --metrics-global-labels label1=value1,label2=value2
    #[arg(long, env, default_value = "")]
    metrics_global_labels: String,

    /// Add the hostname as a label to all Prometheus metrics
    #[arg(long, env, default_value = "false")]
    metrics_host_label: bool,

    /// Maximum backoff allowed for upstream connections
    #[arg(long, env, default_value = "20000")]
    subscriber_max_interval_ms: u64,

    /// Interval in milliseconds between ping messages sent to upstream servers to detect unresponsive connections
    #[arg(long, env, default_value = "2000")]
    subscriber_ping_interval_ms: u64,

    /// Timeout in milliseconds to wait for pong responses from upstream servers before considering the connection dead
    #[arg(long, env, default_value = "4000")]
    subscriber_pong_timeout_ms: u64,

    #[arg(
        long,
        env,
        default_value = "false",
        help = "Allow unauthenticated access to endpoints even if api-keys are provided"
    )]
    public_access_enabled: bool,

    #[arg(long, env, default_value = "false", help = "Enable ping/pong client health checks")]
    client_ping_enabled: bool,

    #[arg(
        long,
        env,
        default_value = "15000",
        help = "Interval in milliseconds to send ping messages to clients"
    )]
    client_ping_interval_ms: u64,

    #[arg(
        long,
        env,
        default_value = "30000",
        help = "Timeout in milliseconds to wait for pong response from clients"
    )]
    client_pong_timeout_ms: u64,

    #[arg(
        long,
        env,
        default_value = "1000",
        help = "Timeout in milliseconds for sending messages to clients"
    )]
    client_send_timeout_ms: u64,

    #[arg(
        long,
        env,
        default_value = "55",
        help = "Number of flashblocks to retain in the replay ring buffer (~10s replay window)"
    )]
    ring_buffer_capacity: usize,
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    let args = Args::parse();

    LogConfig::from(args.log).init_tracing_subscriber().expect("failed to initialize tracing");

    let api_keys: Vec<String> = args.api_keys.into_iter().filter(|s| !s.is_empty()).collect();
    let authentication = if api_keys.is_empty() {
        None
    } else {
        match Authentication::try_from(api_keys) {
            Ok(auth) => Some(auth),
            Err(e) => {
                panic!("Failed to parse API Keys: {e}")
            }
        }
    };

    if args.metrics {
        info!(message = "starting metrics server", address = args.metrics_addr.to_string());

        let mut builder = PrometheusBuilder::new().with_http_listener(args.metrics_addr);

        if args.metrics_host_label {
            let hostname = hostname::get()
                .expect("could not find hostname")
                .into_string()
                .expect("could not convert hostname to string");
            builder = builder.add_global_label("hostname", hostname);
        }

        for (key, value) in parse_global_metrics(args.metrics_global_labels) {
            builder = builder.add_global_label(key, value);
        }

        builder.install().expect("failed to setup Prometheus endpoint")
    }

    // Validate that we have at least one upstream URI
    if args.upstream_ws.is_empty() {
        error!(message = "no upstream URIs provided");
        panic!("No upstream URIs provided");
    }

    info!(message = "using upstream URIs", uris = ?args.upstream_ws);

    let metrics = Arc::new(Metrics::default());
    let metrics_clone = Arc::clone(&metrics);

    let ring_buffer = Arc::new(RwLock::new(FlashblocksRingBuffer::new(args.ring_buffer_capacity)));

    let (send, _rec) = broadcast::channel(args.message_buffer_size);
    let sender = send.clone();

    let ring_buffer_listener = Arc::clone(&ring_buffer);
    let listener = move |data: String| {
        trace!(message = "received data", data = data);
        // Subtract one from receiver count, as we have to keep one receiver open at all times (see _rec)
        // to avoid the channel being closed. However this is not an active client connection.
        metrics_clone.active_connections.set((send.receiver_count() - 1) as f64);

        let message_data = if args.enable_compression {
            let data_bytes = data.as_bytes();
            let mut compressed_data_bytes = Vec::new();
            {
                let mut compressor =
                    brotli::CompressorWriter::new(&mut compressed_data_bytes, 4096, 5, 22);
                compressor.write_all(data_bytes).unwrap();
            }
            compressed_data_bytes
        } else {
            data.as_bytes().to_vec()
        };

        // Push to ring buffer before broadcasting.
        ring_buffer_listener.write().unwrap().push(&data, message_data.clone());

        match send.send(message_data.into()) {
            Ok(_) => {
                metrics_clone.broadcast_queue_size.set(send.len() as f64);
            }
            Err(e) => error!(message = "failed to send data", error = e.to_string()),
        }
    };

    let token = CancellationToken::new();
    let mut subscriber_tasks = Vec::new();

    // Start a subscriber for each upstream URI
    for (index, uri) in args.upstream_ws.iter().enumerate() {
        let uri_clone = uri.clone();
        let listener_clone = listener.clone();
        let token_clone = token.clone();
        let metrics_clone = Arc::clone(&metrics);

        let options = SubscriberOptions::default()
            .with_max_backoff_interval(Duration::from_millis(args.subscriber_max_interval_ms))
            .with_ping_interval(Duration::from_millis(args.subscriber_ping_interval_ms))
            .with_pong_timeout(Duration::from_millis(args.subscriber_pong_timeout_ms))
            .with_backoff_initial_interval(Duration::from_millis(500))
            .with_initial_grace_period(Duration::from_secs(5));

        let mut subscriber =
            WebsocketSubscriber::new(uri_clone.clone(), listener_clone, metrics_clone, options);

        let task = tokio::spawn(async move {
            info!(message = "starting subscriber", index = index, uri = uri_clone.to_string());
            subscriber.run(token_clone).await;
        });

        subscriber_tasks.push(task);
    }

    let ping_task = if args.client_ping_enabled {
        let ping_sender = sender.clone();
        let ping_token = token.clone();
        let ping_interval = args.client_ping_interval_ms;
        let ping_metrics = Arc::clone(&metrics);

        tokio::spawn(async move {
            let mut interval = interval(Duration::from_millis(ping_interval));
            info!(message = "starting ping sender", interval_ms = ping_interval);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        match ping_sender.send(Message::Ping(vec![].into())) {
                            Ok(_) => {
                                trace!(message = "sent ping to all clients");
                                ping_metrics.broadcast_queue_size.set(ping_sender.len() as f64);
                            }
                            Err(e) => error!(message = "failed to send ping", error = e.to_string()),
                        }
                    }
                    _ = ping_token.cancelled() => {
                        info!(message = "ping sender shutting down");
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
        args.enable_compression,
        args.client_ping_enabled,
        args.client_pong_timeout_ms,
        Duration::from_millis(args.client_send_timeout_ms),
        ring_buffer,
    );

    let rate_limiter: Arc<dyn RateLimit> = Arc::new(InMemoryRateLimit::new(
        args.instance_connection_limit,
        args.per_ip_connection_limit,
    ));

    let server = Server::new(
        args.listen_addr,
        registry.clone(),
        metrics,
        rate_limiter,
        authentication,
        args.ip_addr_http_header,
        args.public_access_enabled,
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
}

fn parse_global_metrics(metrics: String) -> Vec<(String, String)> {
    let mut result = Vec::new();

    for metric in metrics.split(',') {
        if metric.is_empty() {
            continue;
        }

        let parts = metric.splitn(2, '=').map(|s| s.to_string()).collect::<Vec<String>>();

        if parts.len() != 2 {
            warn!(message = "malformed global metric: invalid count", metric = metric);
            continue;
        }

        let label = parts[0].clone();
        let value = parts[1].clone();

        if label.is_empty() || value.is_empty() {
            warn!(message = "malformed global metric: empty value", metric = metric);
            continue;
        }

        result.push((label, value));
    }

    result
}

#[cfg(test)]
mod test {
    use crate::parse_global_metrics;

    #[test]
    fn test_parse_global_metrics() {
        assert_eq!(parse_global_metrics(String::new()), Vec::<(String, String)>::new(),);

        assert_eq!(parse_global_metrics("key=value".into()), vec![("key".into(), "value".into())]);

        assert_eq!(
            parse_global_metrics("key=value,key2=value2".into()),
            vec![("key".into(), "value".into()), ("key2".into(), "value2".into())],
        );

        assert_eq!(parse_global_metrics("gibberish".into()), Vec::new());

        assert_eq!(
            parse_global_metrics("key=value,key2=,".into()),
            vec![("key".into(), "value".into())],
        );
    }
}
