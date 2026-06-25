//! Collected metrics for the websocket proxy.

base_metrics::define_metrics! {
    websocket_proxy
    #[describe("Messages sent to clients")]
    sent_messages: counter,
    #[describe("Count of messages that were unable to be sent")]
    failed_messages: counter,
    #[describe("Duration of message send operations")]
    message_send_duration: histogram,
    #[describe("Current size of the broadcast message queue")]
    broadcast_queue_size: gauge,
    #[describe("Count of new connections opened")]
    new_connections: counter,
    #[describe("Count of number of connections closed")]
    closed_connections: counter,
    #[describe("Count the number of connections which lagged and then disconnected")]
    lagged_connections: counter,
    #[describe("Number of client connections currently open")]
    active_connections: gauge,
    #[describe("Count of requests rate limited due to per-IP limits")]
    per_ip_rate_limited_requests: counter,
    #[describe("Count of requests rate limited due to global instance limits")]
    global_rate_limited_requests: counter,
    #[describe("Count of unauthorized requests with invalid API keys")]
    unauthorized_requests: counter,
    #[describe("Count of times upstream receiver was closed/errored")]
    upstream_errors: counter,
    #[describe("Number of active upstream connections")]
    upstream_connections: gauge,
    #[describe("Number of upstream connection attempts")]
    upstream_connection_attempts: counter,
    #[describe("Number of successful upstream connections")]
    upstream_connection_successes: counter,
    #[describe("Number of failed upstream connection attempts")]
    upstream_connection_failures: counter,
    #[describe("Total bytes broadcasted to clients")]
    bytes_broadcasted: counter,
    #[describe("Total brotli-compressed bytes placed on the broadcast channel (pre-fan-out, counted once per message)")]
    bytes_compressed: counter,
    #[describe("Count of clients disconnected due to pong timeout")]
    client_pong_disconnects: counter,
    #[describe("Number of ping attempts sent to upstream")]
    ping_attempts: counter,
    #[describe("Number of ping failures")]
    ping_failures: counter,
    #[describe("Number of pings sent to upstream")]
    ping_sent: counter,
    #[describe("Connections by app")]
    #[label(app)]
    connections_by_app: counter,
    #[describe("Messages received from upstream")]
    #[label(upstream)]
    upstream_messages: counter,
}
