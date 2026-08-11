use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use alloy_rpc_types_engine::PayloadId;
use base_node_core::BaseEngineTypes;
use reth_payload_builder::{BuildNewPayload, PayloadBuilderHandle, PayloadServiceCommand};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::{PayloadKind, PayloadTypes};
use tokio::sync::mpsc;
use tracing::info;

use crate::RoutingConfig;

const FLASHBLOCKS_BUILDER: &str = "flashblocks";
const BASIC_BUILDER: &str = "basic";

#[cfg(debug_assertions)]
static DEBUG_DISPATCH_FLASHBLOCKS: AtomicU64 = AtomicU64::new(0);
#[cfg(debug_assertions)]
static DEBUG_DISPATCH_BASIC: AtomicU64 = AtomicU64::new(0);

/// Shared health state for one inner service.
#[derive(Debug, Clone)]
pub struct HealthState {
    /// True when the service is healthy.
    pub healthy: Arc<AtomicBool>,
}

impl Default for HealthState {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthState {
    /// Creates a healthy state.
    pub fn new() -> Self {
        Self { healthy: Arc::new(AtomicBool::new(true)) }
    }

    /// Marks the service as unavailable.
    pub fn mark_unavailable(&self) {
        self.healthy.store(false, Ordering::Relaxed);
    }

    /// Returns whether the service is healthy.
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }
}

/// Error used to report selected builder unavailability.
#[derive(Debug, thiserror::Error)]
#[error("payload builder unavailable: {builder}")]
pub struct BuilderUnavailableError {
    /// Builder label.
    pub builder: &'static str,
}

/// Boxed future resolving to a built payload, as returned by `resolve_kind`.
pub type ResolveFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = Result<<BaseEngineTypes as PayloadTypes>::BuiltPayload, PayloadBuilderError>,
            > + Send,
    >,
>;

/// Router that multiplexes one outer payload-builder handle over flashblocks + basic shadow.
#[derive(Debug, Clone)]
pub struct MultiplexRouter {
    /// Flashblocks handle used for all reads and selected build results.
    pub flashblocks_handle: PayloadBuilderHandle<BaseEngineTypes>,
    /// Basic handle used only for shadow build fan-out.
    pub basic_handle: PayloadBuilderHandle<BaseEngineTypes>,
    /// Flashblocks health state.
    pub flashblocks_health: HealthState,
    /// Basic health state.
    pub basic_health: HealthState,
    /// Deadline used to flag slow selected `getPayload` resolutions.
    pub getpayload_deadline: std::time::Duration,
}

impl MultiplexRouter {
    /// Creates a new router.
    pub const fn new(
        flashblocks_handle: PayloadBuilderHandle<BaseEngineTypes>,
        basic_handle: PayloadBuilderHandle<BaseEngineTypes>,
        flashblocks_health: HealthState,
        basic_health: HealthState,
        config: RoutingConfig,
    ) -> Self {
        Self {
            flashblocks_handle,
            basic_handle,
            flashblocks_health,
            basic_health,
            getpayload_deadline: config.getpayload_deadline,
        }
    }

    /// Runs the router command loop.
    pub async fn run(
        self,
        mut rx: mpsc::UnboundedReceiver<PayloadServiceCommand<BaseEngineTypes>>,
    ) {
        while let Some(command) = rx.recv().await {
            let router = self.clone();
            tokio::spawn(async move {
                router.handle_command(command).await;
            });
        }
    }

    /// Handles a single command.
    pub async fn handle_command(&self, command: PayloadServiceCommand<BaseEngineTypes>) {
        match command {
            PayloadServiceCommand::BuildNewPayload(input, _span, tx) => {
                self.handle_build_new_payload(*input, tx).await;
            }
            PayloadServiceCommand::BestPayload(payload_id, tx) => {
                self.handle_best_payload(payload_id, tx).await;
            }
            PayloadServiceCommand::PayloadTimestamp(payload_id, tx) => {
                self.handle_payload_timestamp(payload_id, tx).await;
            }
            PayloadServiceCommand::Resolve(payload_id, kind, tx) => {
                self.handle_resolve(payload_id, kind, tx).await;
            }
            PayloadServiceCommand::Subscribe(tx) => {
                self.handle_subscribe(tx).await;
            }
        }
    }

    /// Handles build fan-out.
    pub async fn handle_build_new_payload(
        &self,
        input: BuildNewPayload<<BaseEngineTypes as PayloadTypes>::PayloadAttributes>,
        tx: tokio::sync::oneshot::Sender<Result<PayloadId, PayloadBuilderError>>,
    ) {
        let payload_id = input.payload_id();

        let basic_input = input;
        let flashblocks_input = BuildNewPayload {
            attributes: basic_input.attributes.clone(),
            parent_hash: basic_input.parent_hash,
            cache: None,
            state_root_handle: None,
        };

        Self::inc_dispatch_metric(FLASHBLOCKS_BUILDER);
        Self::inc_dispatch_metric(BASIC_BUILDER);
        Self::inc_selected_build_metric(FLASHBLOCKS_BUILDER);

        let flashblocks_rx = self.flashblocks_handle.send_new_payload(flashblocks_input);
        let basic_rx = self.basic_handle.send_new_payload(basic_input);

        let (flashblocks_result, basic_result) = tokio::join!(flashblocks_rx, basic_rx);
        let selected_result = flashblocks_result.unwrap_or_else(|_| {
            self.flashblocks_health.mark_unavailable();
            Self::set_service_health_metric(FLASHBLOCKS_BUILDER, false);
            Err(Self::unavailable_error(FLASHBLOCKS_BUILDER))
        });
        let shadow_result = basic_result.unwrap_or_else(|_| {
            self.basic_health.mark_unavailable();
            Self::set_service_health_metric(BASIC_BUILDER, false);
            Err(Self::unavailable_error(BASIC_BUILDER))
        });

        Self::inc_shadow_metric(BASIC_BUILDER, shadow_result.is_ok());

        info!(
            builder = FLASHBLOCKS_BUILDER,
            payload_id = ?payload_id,
            selected = true,
            result = if selected_result.is_ok() { "ok" } else { "err" },
            "multiplex build request completed"
        );

        let _ = tx.send(selected_result);
    }

    /// Handles best payload lookup.
    pub async fn handle_best_payload(
        &self,
        payload_id: PayloadId,
        tx: tokio::sync::oneshot::Sender<
            Option<Result<<BaseEngineTypes as PayloadTypes>::BuiltPayload, PayloadBuilderError>>,
        >,
    ) {
        let result = self.flashblocks_handle.best_payload(payload_id).await;
        let mapped = self.map_flashblocks_read_result(result);
        let _ = tx.send(mapped);
    }

    /// Handles payload timestamp lookup.
    pub async fn handle_payload_timestamp(
        &self,
        payload_id: PayloadId,
        tx: tokio::sync::oneshot::Sender<Option<Result<u64, PayloadBuilderError>>>,
    ) {
        let result = self.flashblocks_handle.payload_timestamp(payload_id).await;
        let mapped = self.map_flashblocks_read_result(result);
        let _ = tx.send(mapped);
    }

    /// Handles payload resolve.
    pub async fn handle_resolve(
        &self,
        payload_id: PayloadId,
        kind: PayloadKind,
        tx: tokio::sync::oneshot::Sender<Option<ResolveFuture>>,
    ) {
        let handle = self.flashblocks_handle.clone();
        let health = self.flashblocks_health.clone();
        let deadline = self.getpayload_deadline;
        let future = async move {
            let started = Instant::now();
            let result = handle.resolve_kind(payload_id, kind).await;
            let elapsed = started.elapsed();

            Self::record_selected_getpayload_latency(elapsed.as_secs_f64());
            if elapsed > deadline {
                Self::inc_selected_deadline_miss();
            }

            match result {
                Some(Ok(payload)) => Ok(payload),
                Some(Err(PayloadBuilderError::ChannelClosed)) => {
                    health.mark_unavailable();
                    Self::set_service_health_metric(FLASHBLOCKS_BUILDER, false);
                    Err(Self::unavailable_error(FLASHBLOCKS_BUILDER))
                }
                Some(Err(err)) => Err(err),
                None => {
                    if !health.is_healthy() {
                        Err(Self::unavailable_error(FLASHBLOCKS_BUILDER))
                    } else {
                        Err(PayloadBuilderError::MissingPayload)
                    }
                }
            }
        };

        let _ = tx.send(Some(Box::pin(future)));
    }

    /// Handles subscriptions.
    pub async fn handle_subscribe(
        &self,
        tx: tokio::sync::oneshot::Sender<
            tokio::sync::broadcast::Receiver<
                reth_payload_builder_primitives::Events<BaseEngineTypes>,
            >,
        >,
    ) {
        if !self.flashblocks_health.is_healthy() {
            return;
        }

        match self.flashblocks_handle.subscribe().await {
            Ok(events) => {
                let _ = tx.send(events.receiver);
            }
            Err(err) => {
                if matches!(err, PayloadBuilderError::ChannelClosed) {
                    self.flashblocks_health.mark_unavailable();
                    Self::set_service_health_metric(FLASHBLOCKS_BUILDER, false);
                }
            }
        }
    }

    /// Maps flashblocks read results with unavailable conversion.
    pub fn map_flashblocks_read_result<T>(
        &self,
        result: Option<Result<T, PayloadBuilderError>>,
    ) -> Option<Result<T, PayloadBuilderError>> {
        match result {
            Some(Err(PayloadBuilderError::ChannelClosed)) => {
                self.flashblocks_health.mark_unavailable();
                Self::set_service_health_metric(FLASHBLOCKS_BUILDER, false);
                Some(Err(Self::unavailable_error(FLASHBLOCKS_BUILDER)))
            }
            None if !self.flashblocks_health.is_healthy() => {
                Some(Err(Self::unavailable_error(FLASHBLOCKS_BUILDER)))
            }
            other => other,
        }
    }

    /// Creates unavailable error.
    pub fn unavailable_error(builder: &'static str) -> PayloadBuilderError {
        PayloadBuilderError::other(BuilderUnavailableError { builder })
    }

    /// Increments dispatch metric.
    pub fn inc_dispatch_metric(builder: &'static str) {
        #[cfg(debug_assertions)]
        if builder == FLASHBLOCKS_BUILDER {
            DEBUG_DISPATCH_FLASHBLOCKS.fetch_add(1, Ordering::Relaxed);
        } else if builder == BASIC_BUILDER {
            DEBUG_DISPATCH_BASIC.fetch_add(1, Ordering::Relaxed);
        }

        metrics::counter!("mux_builds_dispatched_total", "builder" => builder).increment(1);
    }

    /// Returns the in-process debug dispatch counter for flashblocks.
    #[cfg(debug_assertions)]
    pub fn debug_flashblocks_dispatch_count() -> u64 {
        DEBUG_DISPATCH_FLASHBLOCKS.load(Ordering::Relaxed)
    }

    /// Returns the in-process debug dispatch counter for basic.
    #[cfg(debug_assertions)]
    pub fn debug_basic_dispatch_count() -> u64 {
        DEBUG_DISPATCH_BASIC.load(Ordering::Relaxed)
    }

    /// Resets in-process debug dispatch counters.
    #[cfg(debug_assertions)]
    pub fn debug_reset_dispatch_counts() {
        DEBUG_DISPATCH_FLASHBLOCKS.store(0, Ordering::Relaxed);
        DEBUG_DISPATCH_BASIC.store(0, Ordering::Relaxed);
    }

    /// Increments selected metric.
    pub fn inc_selected_build_metric(builder: &'static str) {
        metrics::counter!("mux_selected_builds_total", "builder" => builder).increment(1);
    }

    /// Increments shadow outcome metric.
    pub fn inc_shadow_metric(builder: &'static str, ok: bool) {
        let result = if ok { "ok" } else { "err" };
        metrics::counter!(
            "mux_shadow_outcome_total",
            "builder" => builder,
            "result" => result
        )
        .increment(1);
    }

    /// Sets service health gauge.
    pub fn set_service_health_metric(builder: &'static str, healthy: bool) {
        metrics::gauge!("mux_service_health", "builder" => builder).set(if healthy {
            1.0
        } else {
            0.0
        });
    }

    /// Records getPayload latency for selected route.
    pub fn record_selected_getpayload_latency(seconds: f64) {
        metrics::histogram!("mux_selected_getpayload_latency_seconds").record(seconds);
    }

    /// Increments selected deadline miss counter.
    pub fn inc_selected_deadline_miss() {
        metrics::counter!("mux_selected_deadline_miss_total").increment(1);
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use reth_payload_builder::PayloadBuilderHandle;
    use reth_payload_builder_primitives::Events;
    use tokio::sync::{broadcast, mpsc};

    use super::*;

    fn test_router() -> (
        MultiplexRouter,
        mpsc::UnboundedReceiver<PayloadServiceCommand<BaseEngineTypes>>,
        mpsc::UnboundedReceiver<PayloadServiceCommand<BaseEngineTypes>>,
    ) {
        let (flash_tx, flash_rx) = mpsc::unbounded_channel();
        let (basic_tx, basic_rx) = mpsc::unbounded_channel();
        let router = MultiplexRouter::new(
            PayloadBuilderHandle::new(flash_tx),
            PayloadBuilderHandle::new(basic_tx),
            HealthState::new(),
            HealthState::new(),
            RoutingConfig::default(),
        );
        (router, flash_rx, basic_rx)
    }

    fn sample_input() -> BuildNewPayload<<BaseEngineTypes as PayloadTypes>::PayloadAttributes> {
        BuildNewPayload {
            attributes: base_execution_payload_builder::BasePayloadBuilderAttributes::default(),
            parent_hash: B256::ZERO,
            cache: None,
            state_root_handle: None,
        }
    }

    fn payload_id_from_byte(value: u8) -> PayloadId {
        let mut input = sample_input();
        input.parent_hash = B256::repeat_byte(value);
        input.payload_id()
    }

    #[tokio::test]
    async fn build_fans_to_both_and_selects_flashblocks() {
        let (router, mut flash_rx, mut basic_rx) = test_router();
        let (tx, rx) = tokio::sync::oneshot::channel();

        let input = sample_input();
        let payload_id = input.payload_id();
        tokio::spawn(async move {
            router.handle_build_new_payload(input, tx).await;
        });

        let flash_cmd = flash_rx.recv().await.expect("flash cmd");
        let basic_cmd = basic_rx.recv().await.expect("basic cmd");
        let mut flash_seen = false;
        let mut basic_seen = false;

        if let PayloadServiceCommand::BuildNewPayload(input, _, tx) = flash_cmd {
            flash_seen = true;
            assert_eq!(input.payload_id(), payload_id);
            assert!(input.cache.is_none());
            assert!(input.state_root_handle.is_none());
            tx.send(Ok(payload_id)).expect("flash response");
        }

        if let PayloadServiceCommand::BuildNewPayload(input, _, tx) = basic_cmd {
            basic_seen = true;
            assert_eq!(input.payload_id(), payload_id);
            tx.send(Err(PayloadBuilderError::MissingPayload))
                .expect("basic response");
        }

        assert!(flash_seen);
        assert!(basic_seen);
        assert!(rx.await.expect("selected response").is_ok());
    }

    #[tokio::test]
    async fn best_payload_reads_flashblocks_only() {
        let (router, mut flash_rx, mut basic_rx) = test_router();
        let payload_id = payload_id_from_byte(7);
        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            router.handle_best_payload(payload_id, tx).await;
        });

        let flash_cmd = flash_rx.recv().await.expect("flash command");
        assert!(basic_rx.try_recv().is_err());
        if let PayloadServiceCommand::BestPayload(inner_payload_id, tx) = flash_cmd {
            assert_eq!(inner_payload_id, payload_id);
            tx.send(None).expect("send flash response");
        } else {
            panic!("expected BestPayload command");
        }

        assert!(rx.await.expect("best response").is_none());
    }

    #[tokio::test]
    async fn payload_timestamp_reads_flashblocks_only() {
        let (router, mut flash_rx, mut basic_rx) = test_router();
        let payload_id = payload_id_from_byte(9);
        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            router.handle_payload_timestamp(payload_id, tx).await;
        });

        let flash_cmd = flash_rx.recv().await.expect("flash command");
        assert!(basic_rx.try_recv().is_err());
        if let PayloadServiceCommand::PayloadTimestamp(inner_payload_id, tx) = flash_cmd {
            assert_eq!(inner_payload_id, payload_id);
            tx.send(None).expect("send flash response");
        } else {
            panic!("expected PayloadTimestamp command");
        }

        assert!(rx.await.expect("timestamp response").is_none());
    }

    #[tokio::test]
    async fn subscribe_uses_flashblocks_only() {
        let (router, mut flash_rx, mut basic_rx) = test_router();
        let (sub_tx, sub_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            router.handle_subscribe(sub_tx).await;
        });

        let flash_cmd = flash_rx.recv().await.expect("flash subscribe command");
        assert!(basic_rx.try_recv().is_err());
        if let PayloadServiceCommand::Subscribe(tx) = flash_cmd {
            let (events_tx, events_rx) = broadcast::channel(2);
            tx.send(events_rx).expect("send flash receiver");
            let mut sub = sub_rx.await.expect("outer subscribe receiver");
            events_tx
                .send(Events::Attributes(sample_input().attributes))
                .expect("send flash event");
            assert!(sub.recv().await.is_ok());
        } else {
            panic!("expected Subscribe command");
        }
    }

    #[tokio::test]
    async fn resolve_uses_flashblocks_only() {
        let (router, mut flash_rx, mut basic_rx) = test_router();
        let payload_id = payload_id_from_byte(11);
        let (resolve_tx, resolve_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            router
                .handle_resolve(payload_id, PayloadKind::Earliest, resolve_tx)
                .await;
        });

        let future = resolve_rx
            .await
            .expect("resolve response")
            .expect("resolve future should be present");

        let resolve_task = tokio::spawn(future);

        let flash_cmd = flash_rx.recv().await.expect("flash resolve command");
        assert!(basic_rx.try_recv().is_err());
        if let PayloadServiceCommand::Resolve(inner_payload_id, _, tx) = flash_cmd {
            assert_eq!(inner_payload_id, payload_id);
            assert!(tx.send(None).is_ok(), "send flash resolve response");
        } else {
            panic!("expected Resolve command");
        }

        let resolved = resolve_task.await.expect("resolve task join");
        assert!(matches!(resolved, Err(PayloadBuilderError::MissingPayload)));
    }
}
