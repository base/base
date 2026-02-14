use metrics::{Counter, Gauge, Histogram, counter};
use metrics_derive::Metrics;

/// Collected metrics for the websocket proxy.
#[derive(Metrics)]
#[metrics(scope = "websocket_proxy")]
pub struct Metrics {
    /// Messages sent to clients.
    #[metric(describe = "Messages sent to clients")]
    pub sent_messages: Counter,

    /// Count of messages that were unable to be sent.
    #[metric(describe = "Count of messages that were unable to be sent")]
    pub failed_messages: Counter,

    /// Duration of message send operations.
    #[metric(describe = "Duration of message send operations")]
    pub message_send_duration: Histogram,

    /// Current size of the broadcast message queue.
    #[metric(describe = "Current size of the broadcast message queue")]
    pub broadcast_queue_size: Gauge,

    /// Count of new connections opened.
    #[metric(describe = "Count of new connections opened")]
    pub new_connections: Counter,

    /// Count of connections closed.
    #[metric(describe = "Count of number of connections closed")]
    pub closed_connections: Counter,

    /// Count of connections that lagged and then disconnected.
    #[metric(describe = "Count the number of connections which lagged and then disconnected")]
    pub lagged_connections: Counter,

    /// Number of client connections currently open.
    #[metric(describe = "Number of client connections currently open")]
    pub active_connections: Gauge,

    /// Count of requests rate limited due to per-IP limits.
    #[metric(describe = "Count of requests rate limited due to per-IP limits")]
    pub per_ip_rate_limited_requests: Counter,

    /// Count of requests rate limited due to global instance limits.
    #[metric(describe = "Count of requests rate limited due to global instance limits")]
    pub global_rate_limited_requests: Counter,

    /// Count of unauthorized requests with invalid API keys.
    #[metric(describe = "Count of unauthorized requests with invalid API keys")]
    pub unauthorized_requests: Counter,

    /// Count of times the upstream receiver was closed or errored.
    #[metric(describe = "Count of times upstream receiver was closed/errored")]
    pub upstream_errors: Counter,

    /// Number of active upstream connections.
    #[metric(describe = "Number of active upstream connections")]
    pub upstream_connections: Gauge,

    /// Number of upstream connection attempts.
    #[metric(describe = "Number of upstream connection attempts")]
    pub upstream_connection_attempts: Counter,

    /// Number of successful upstream connections.
    #[metric(describe = "Number of successful upstream connections")]
    pub upstream_connection_successes: Counter,

    /// Number of failed upstream connection attempts.
    #[metric(describe = "Number of failed upstream connection attempts")]
    pub upstream_connection_failures: Counter,

    /// Total bytes broadcasted to clients.
    #[metric(describe = "Total bytes broadcasted to clients")]
    pub bytes_broadcasted: Counter,

    /// Count of clients disconnected due to pong timeout.
    #[metric(describe = "Count of clients disconnected due to pong timeout")]
    pub client_pong_disconnects: Counter,

    /// Number of ping attempts sent to upstream.
    #[metric(describe = "Number of ping attempts sent to upstream")]
    pub ping_attempts: Counter,

    /// Number of ping failures.
    #[metric(describe = "Number of ping failures")]
    pub ping_failures: Counter,

    /// Number of pings sent to upstream.
    #[metric(describe = "Number of pings sent to upstream")]
    pub ping_sent: Counter,
}

impl Metrics {
    /// Records a proxy connection for the given application name.
    pub fn proxy_connections_by_app(&self, app: &str) {
        counter!("websocket_proxy.connections_by_app", "app" => app.to_owned()).increment(1);
    }

    /// Records a message received from the given upstream source.
    pub fn message_received_from_upstream(&self, upstream: &str) {
        counter!("websocket_proxy.upstream_messages", "upstream" => upstream.to_owned())
            .increment(1);
    }
}
