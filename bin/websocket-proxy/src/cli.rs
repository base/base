use std::{net::SocketAddr, time::Duration};

use clap::Parser;
use websocket_proxy::{
    Authentication, ClientPingConfig, MetricsServerConfig, SubscriberOptions,
    WebsocketProxyConfig,
};

use crate::LogArgs;

/// CLI entry point for the websocket proxy.
#[derive(Parser, Debug)]
#[command(author, version, about)]
pub(crate) struct Args {
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
    upstream_ws: Vec<axum::http::Uri>,

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
    pub log: LogArgs,

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
}

impl TryFrom<Args> for WebsocketProxyConfig {
    type Error = anyhow::Error;

    fn try_from(args: Args) -> Result<Self, Self::Error> {
        let api_keys: Vec<String> =
            args.api_keys.into_iter().filter(|s| !s.is_empty()).collect();

        let authentication = if api_keys.is_empty() {
            None
        } else {
            Some(Authentication::try_from(api_keys).map_err(|e| anyhow::anyhow!("{e}"))?)
        };

        let metrics_config = if args.metrics {
            let mut global_labels = parse_global_metrics(args.metrics_global_labels);

            if args.metrics_host_label {
                let hostname = hostname::get()
                    .expect("could not find hostname")
                    .into_string()
                    .expect("could not convert hostname to string");
                global_labels.push(("hostname".to_string(), hostname));
            }

            Some(MetricsServerConfig { port: args.metrics_addr.port(), global_labels })
        } else {
            None
        };

        let subscriber_options = SubscriberOptions::default()
            .with_max_backoff_interval(Duration::from_millis(args.subscriber_max_interval_ms))
            .with_ping_interval(Duration::from_millis(args.subscriber_ping_interval_ms))
            .with_pong_timeout(Duration::from_millis(args.subscriber_pong_timeout_ms))
            .with_backoff_initial_interval(Duration::from_millis(500))
            .with_initial_grace_period(Duration::from_secs(5));

        Ok(Self {
            listen_addr: args.listen_addr,
            upstream_ws: args.upstream_ws,
            message_buffer_size: args.message_buffer_size,
            instance_connection_limit: args.instance_connection_limit,
            per_ip_connection_limit: args.per_ip_connection_limit,
            enable_compression: args.enable_compression,
            ip_addr_http_header: args.ip_addr_http_header,
            subscriber_options,
            authentication,
            metrics_config,
            public_access_enabled: args.public_access_enabled,
            client_ping: ClientPingConfig {
                enabled: args.client_ping_enabled,
                interval_secs: args.client_ping_interval_ms,
                pong_timeout_ms: args.client_pong_timeout_ms,
            },
            client_send_timeout_ms: args.client_send_timeout_ms,
            log: args.log.into(),
        })
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
            tracing::warn!(metric = metric, "malformed global metric: invalid count");
            continue;
        }

        let label = parts[0].clone();
        let value = parts[1].clone();

        if label.is_empty() || value.is_empty() {
            tracing::warn!(metric = metric, "malformed global metric: empty value");
            continue;
        }

        result.push((label, value));
    }

    result
}

#[cfg(test)]
mod test {
    use super::parse_global_metrics;

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
