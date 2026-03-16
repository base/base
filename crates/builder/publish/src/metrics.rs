use core::fmt::{Debug, Formatter};
use std::time::Duration;

/// Trait for observing WebSocket publisher events.
///
/// Implementors receive callbacks for key lifecycle events so that
/// metrics, logging, or test assertions can be injected without coupling
/// the publisher internals to a specific metrics library.
pub trait PublisherMetrics: Send + Sync {
    /// Called when a message is successfully sent to a subscriber.
    fn on_message_sent(&self);

    /// Called each time a replay message is sent during reconnection replay.
    ///
    /// Defaults to calling [`on_message_sent`](Self::on_message_sent) for
    /// backwards compatibility.
    fn on_replay_message_sent(&self) {
        self.on_message_sent();
    }

    /// Called when a new WebSocket connection is accepted.
    fn on_connection_opened(&self);
    /// Called when a WebSocket connection closes, with its total duration.
    fn on_connection_closed(&self, duration: Duration);
    /// Called when the broadcast channel lags and `skipped` messages are dropped.
    fn on_lagged(&self, skipped: u64);
    /// Called after serialising a payload, with the byte size.
    fn on_payload_size(&self, size: usize);
    /// Called when a WebSocket send operation fails.
    fn on_send_error(&self);
    /// Called when a WebSocket handshake fails.
    fn on_handshake_error(&self);

    /// Called when a subscriber's resume position is older than the ring
    /// buffer's oldest entry, causing a silent gap in the replay.
    ///
    /// Useful for tuning ring buffer capacity.
    fn on_replay_stale_position(&self) {}
}

/// No-op implementation of [`PublisherMetrics`].
///
/// All methods are no-ops — useful for unit tests and environments
/// where metrics collection is not required.
#[derive(Debug)]
pub struct NoopPublisherMetrics;

impl PublisherMetrics for NoopPublisherMetrics {
    fn on_message_sent(&self) {}
    fn on_connection_opened(&self) {}
    fn on_connection_closed(&self, _duration: Duration) {}
    fn on_lagged(&self, _skipped: u64) {}
    fn on_payload_size(&self, _size: usize) {}
    fn on_send_error(&self) {}
    fn on_handshake_error(&self) {}
}

/// Concrete [`PublisherMetrics`] implementation backed by the [`metrics`] crate.
///
/// Registers counters, gauges, and histograms under the `base_builder` scope
/// in the global metrics recorder. Created automatically by
/// [`WebSocketPublisher::new`](crate::WebSocketPublisher::new).
pub struct PublishingMetrics {
    messages_sent_count: metrics::Counter,
    replay_messages_sent_count: metrics::Counter,
    replay_stale_position_count: metrics::Counter,
    ws_connections_active: metrics::Gauge,
    ws_lagged_count: metrics::Counter,
    ws_payload_byte_size: metrics::Histogram,
    ws_send_error_count: metrics::Counter,
    ws_handshake_error_count: metrics::Counter,
    ws_connection_duration: metrics::Histogram,
}

impl Default for PublishingMetrics {
    fn default() -> Self {
        Self {
            messages_sent_count: metrics::counter!("base_builder_messages_sent_count"),
            replay_messages_sent_count: metrics::counter!(
                "base_builder_replay_messages_sent_count"
            ),
            replay_stale_position_count: metrics::counter!(
                "base_builder_replay_stale_position_count"
            ),
            ws_connections_active: metrics::gauge!("base_builder_ws_connections_active"),
            ws_lagged_count: metrics::counter!("base_builder_ws_lagged_count"),
            ws_payload_byte_size: metrics::histogram!("base_builder_ws_payload_byte_size"),
            ws_send_error_count: metrics::counter!("base_builder_ws_send_error_count"),
            ws_handshake_error_count: metrics::counter!("base_builder_ws_handshake_error_count"),
            ws_connection_duration: metrics::histogram!("base_builder_ws_connection_duration"),
        }
    }
}

impl Debug for PublishingMetrics {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PublishingMetrics").finish_non_exhaustive()
    }
}

impl PublisherMetrics for PublishingMetrics {
    fn on_message_sent(&self) {
        self.messages_sent_count.increment(1);
    }

    fn on_replay_message_sent(&self) {
        self.replay_messages_sent_count.increment(1);
    }

    fn on_connection_opened(&self) {
        self.ws_connections_active.increment(1.0);
    }

    fn on_connection_closed(&self, duration: Duration) {
        self.ws_connections_active.decrement(1.0);
        self.ws_connection_duration.record(duration.as_secs_f64());
    }

    fn on_lagged(&self, skipped: u64) {
        self.ws_lagged_count.increment(skipped);
    }

    fn on_payload_size(&self, size: usize) {
        self.ws_payload_byte_size.record(size as f64);
    }

    fn on_send_error(&self) {
        self.ws_send_error_count.increment(1);
    }

    fn on_handshake_error(&self) {
        self.ws_handshake_error_count.increment(1);
    }

    fn on_replay_stale_position(&self) {
        self.replay_stale_position_count.increment(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_publisher_metrics_can_be_constructed_and_called() {
        let metrics = NoopPublisherMetrics;
        metrics.on_message_sent();
        metrics.on_replay_message_sent();
        metrics.on_connection_opened();
        metrics.on_connection_closed(Duration::from_secs(10));
        metrics.on_lagged(42);
        metrics.on_payload_size(1024);
        metrics.on_send_error();
        metrics.on_handshake_error();
        metrics.on_replay_stale_position();
    }

    #[test]
    fn publishing_metrics_can_be_constructed_and_called() {
        let metrics = PublishingMetrics::default();
        metrics.on_message_sent();
        metrics.on_replay_message_sent();
        metrics.on_connection_opened();
        metrics.on_connection_closed(Duration::from_secs(10));
        metrics.on_lagged(42);
        metrics.on_payload_size(1024);
        metrics.on_send_error();
        metrics.on_handshake_error();
        metrics.on_replay_stale_position();
    }
}
