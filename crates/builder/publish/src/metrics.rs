/// Trait for observing WebSocket publisher events.
///
/// Implementors receive callbacks for key lifecycle events so that
/// metrics, logging, or test assertions can be injected without coupling
/// the publisher internals to a specific metrics library.
pub trait PublisherMetrics: Send + Sync {
    /// Called when a message is successfully sent to a subscriber.
    fn on_message_sent(&self);
    /// Called when a new WebSocket connection is accepted.
    fn on_connection_opened(&self);
    /// Called when a WebSocket connection closes, with its total duration.
    fn on_connection_closed(&self, duration: std::time::Duration);
    /// Called when the broadcast channel lags and `skipped` messages are dropped.
    fn on_lagged(&self, skipped: u64);
    /// Called after serialising a payload, with the byte size.
    fn on_payload_size(&self, size: usize);
    /// Called when a WebSocket send operation fails.
    fn on_send_error(&self);
    /// Called when a WebSocket handshake fails.
    fn on_handshake_error(&self);
}

base_metrics::define_metrics! {
    base_builder,
    struct = PublishingMetrics,
    #[describe("Total messages sent to subscribers")]
    messages_sent_count: counter,
    #[describe("Active WebSocket connections")]
    ws_connections_active: gauge,
    #[describe("Total lagged messages dropped")]
    ws_lagged_count: counter,
    #[describe("Payload byte size histogram")]
    ws_payload_byte_size: histogram,
    #[describe("Total WebSocket send errors")]
    ws_send_error_count: counter,
    #[describe("Total WebSocket handshake errors")]
    ws_handshake_error_count: counter,
    #[describe("WebSocket connection duration")]
    ws_connection_duration: histogram,
}

impl PublisherMetrics for PublishingMetrics {
    fn on_message_sent(&self) {
        Self::messages_sent_count().increment(1);
    }

    fn on_connection_opened(&self) {
        Self::ws_connections_active().increment(1.0);
    }

    fn on_connection_closed(&self, duration: std::time::Duration) {
        Self::ws_connections_active().decrement(1.0);
        Self::ws_connection_duration().record(duration.as_secs_f64());
    }

    fn on_lagged(&self, skipped: u64) {
        Self::ws_lagged_count().increment(skipped);
    }

    fn on_payload_size(&self, size: usize) {
        Self::ws_payload_byte_size().record(size as f64);
    }

    fn on_send_error(&self) {
        Self::ws_send_error_count().increment(1);
    }

    fn on_handshake_error(&self) {
        Self::ws_handshake_error_count().increment(1);
    }
}

#[cfg(test)]
mod tests {
    use metrics_exporter_prometheus::PrometheusBuilder;

    use super::*;

    #[test]
    fn publishing_metrics_emit_expected_counters() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            PublishingMetrics::messages_sent_count().increment(3);
            PublishingMetrics::ws_lagged_count().increment(7);
            PublishingMetrics::ws_send_error_count().increment(1);
            PublishingMetrics::ws_handshake_error_count().increment(2);
            PublishingMetrics::ws_connections_active().increment(5.0);
            PublishingMetrics::ws_connections_active().decrement(2.0);
            PublishingMetrics::ws_payload_byte_size().record(1024.0);
            PublishingMetrics::ws_connection_duration().record(42.5);
        });

        let rendered = handle.render();
        assert!(rendered.contains("base_builder_messages_sent_count 3"));
        assert!(rendered.contains("base_builder_ws_lagged_count 7"));
        assert!(rendered.contains("base_builder_ws_send_error_count 1"));
        assert!(rendered.contains("base_builder_ws_handshake_error_count 2"));
        assert!(rendered.contains("base_builder_ws_connections_active 3"));
        assert!(rendered.contains("base_builder_ws_payload_byte_size_sum 1024"));
        assert!(rendered.contains("base_builder_ws_connection_duration_sum 42.5"));
    }
}
