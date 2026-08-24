use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use alloy_consensus::BlockHeader;
use alloy_rpc_types_engine::PayloadId;
use base_common_chains::Upgrades;
use base_execution_chainspec::BaseChainSpec;
use base_node_core::BaseEngineTypes;
use reth_payload_builder::{BuildNewPayload, PayloadBuilderHandle, PayloadServiceCommand};
use reth_payload_builder_primitives::{Events, PayloadBuilderError};
use reth_payload_primitives::{PayloadAttributes, PayloadKind, PayloadTypes};
use tokio::sync::{Mutex, broadcast, mpsc};
use tracing::{error, info, warn};

use crate::RoutingConfig;

const FLASHBLOCKS_BUILDER: &str = "flashblocks";
const BASIC_BUILDER: &str = "basic";
const MAX_PAYLOAD_ROUTES: usize = 128;

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
                Output = Result<
                    <BaseEngineTypes as PayloadTypes>::BuiltPayload,
                    PayloadBuilderError,
                >,
            > + Send,
    >,
>;

/// Router that cuts payload selection from flashblocks to basic when Denim activates.
#[derive(Debug, Clone)]
pub struct MultiplexRouter {
    /// Flashblocks payload-builder handle.
    pub flashblocks_handle: PayloadBuilderHandle<BaseEngineTypes>,
    /// Basic payload-builder handle.
    pub basic_handle: PayloadBuilderHandle<BaseEngineTypes>,
    /// Flashblocks health state.
    pub flashblocks_health: HealthState,
    /// Basic health state.
    pub basic_health: HealthState,
    /// Chain spec that owns the Denim activation condition.
    pub chain_spec: Arc<BaseChainSpec>,
    /// Whether recent payload IDs are routed to the basic builder, ordered oldest first.
    pub payload_routes: Arc<Mutex<VecDeque<(PayloadId, bool)>>>,
    /// Deadline used to flag slow selected `getPayload` resolutions.
    pub getpayload_deadline: std::time::Duration,
}

impl MultiplexRouter {
    /// Creates a new router.
    pub fn new(
        flashblocks_handle: PayloadBuilderHandle<BaseEngineTypes>,
        basic_handle: PayloadBuilderHandle<BaseEngineTypes>,
        flashblocks_health: HealthState,
        basic_health: HealthState,
        chain_spec: Arc<BaseChainSpec>,
        config: RoutingConfig,
    ) -> Self {
        Self {
            flashblocks_handle,
            basic_handle,
            flashblocks_health,
            basic_health,
            chain_spec,
            payload_routes: Arc::new(Mutex::new(VecDeque::new())),
            getpayload_deadline: config.getpayload_deadline,
        }
    }

    /// Returns whether Denim selects the basic builder at `timestamp`.
    pub fn basic_selected_at(&self, timestamp: u64) -> bool {
        self.chain_spec.is_denim_active_at_timestamp(timestamp)
    }

    /// Returns the recorded route for a payload, defaulting unknown payloads to flashblocks.
    pub async fn basic_selected_for_payload(&self, payload_id: PayloadId) -> bool {
        self.payload_routes
            .lock()
            .await
            .iter()
            .rev()
            .find_map(|(id, selected_basic)| (*id == payload_id).then_some(*selected_basic))
            .unwrap_or(false)
    }

    /// Records a payload route while bounding abandoned payload state.
    pub async fn record_payload_route(&self, payload_id: PayloadId, selected_basic: bool) {
        let mut routes = self.payload_routes.lock().await;
        if let Some(position) = routes.iter().position(|(id, _)| *id == payload_id) {
            routes.remove(position);
        }
        if routes.len() == MAX_PAYLOAD_ROUTES {
            routes.pop_front();
        }
        routes.push_back((payload_id, selected_basic));
    }

    /// Returns the timestamp represented by a payload-builder event.
    pub fn event_timestamp(event: &Events<BaseEngineTypes>) -> u64 {
        match event {
            Events::Attributes(attributes) => attributes.timestamp(),
            Events::BuiltPayload(payload) => payload.block().timestamp(),
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
        let selected_basic = self.basic_selected_at(input.attributes.timestamp());
        self.record_payload_route(payload_id, selected_basic).await;

        let mut shadow_attributes = input.attributes.clone();
        shadow_attributes.no_tx_pool = true;
        let shadow_input = BuildNewPayload {
            attributes: shadow_attributes,
            parent_hash: input.parent_hash,
            resources: Default::default(),
        };
        let (flashblocks_input, basic_input) =
            if selected_basic { (shadow_input, input) } else { (input, shadow_input) };

        Self::inc_dispatch_metric(FLASHBLOCKS_BUILDER);
        Self::inc_dispatch_metric(BASIC_BUILDER);

        let flashblocks_rx = self.flashblocks_handle.send_new_payload(flashblocks_input);
        let basic_rx = self.basic_handle.send_new_payload(basic_input);

        let (
            selected_rx,
            selected_health,
            selected_builder,
            shadow_rx,
            shadow_health,
            shadow_builder,
        ) = if selected_basic {
            (
                basic_rx,
                self.basic_health.clone(),
                BASIC_BUILDER,
                flashblocks_rx,
                self.flashblocks_health.clone(),
                FLASHBLOCKS_BUILDER,
            )
        } else {
            (
                flashblocks_rx,
                self.flashblocks_health.clone(),
                FLASHBLOCKS_BUILDER,
                basic_rx,
                self.basic_health.clone(),
                BASIC_BUILDER,
            )
        };

        Self::inc_selected_build_metric(selected_builder);
        tokio::spawn(async move {
            let shadow_result = shadow_rx.await.unwrap_or_else(|_| {
                shadow_health.mark_unavailable();
                Self::set_service_health_metric(shadow_builder, false);
                Err(Self::unavailable_error(shadow_builder))
            });
            Self::inc_shadow_metric(shadow_builder, shadow_result.is_ok());
            info!(
                builder = shadow_builder,
                payload_id = ?payload_id,
                selected = false,
                result = if shadow_result.is_ok() { "ok" } else { "err" },
                "multiplex shadow build request completed"
            );
        });

        let selected_result = selected_rx.await.unwrap_or_else(|_| {
            selected_health.mark_unavailable();
            Self::set_service_health_metric(selected_builder, false);
            Err(Self::unavailable_error(selected_builder))
        });
        if selected_result.is_err() {
            let mut routes = self.payload_routes.lock().await;
            if let Some(position) = routes.iter().position(|(id, _)| *id == payload_id) {
                routes.remove(position);
            }
        }

        info!(
            builder = selected_builder,
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
        let selected_basic = self.basic_selected_for_payload(payload_id).await;
        let (result, builder, health) = if selected_basic {
            (self.basic_handle.best_payload(payload_id).await, BASIC_BUILDER, &self.basic_health)
        } else {
            (
                self.flashblocks_handle.best_payload(payload_id).await,
                FLASHBLOCKS_BUILDER,
                &self.flashblocks_health,
            )
        };
        let mapped = self.map_read_result(result, builder, health);
        let _ = tx.send(mapped);
    }

    /// Handles payload timestamp lookup.
    pub async fn handle_payload_timestamp(
        &self,
        payload_id: PayloadId,
        tx: tokio::sync::oneshot::Sender<Option<Result<u64, PayloadBuilderError>>>,
    ) {
        let selected_basic = self.basic_selected_for_payload(payload_id).await;
        let (result, builder, health) = if selected_basic {
            (
                self.basic_handle.payload_timestamp(payload_id).await,
                BASIC_BUILDER,
                &self.basic_health,
            )
        } else {
            (
                self.flashblocks_handle.payload_timestamp(payload_id).await,
                FLASHBLOCKS_BUILDER,
                &self.flashblocks_health,
            )
        };
        let mapped = self.map_read_result(result, builder, health);
        let _ = tx.send(mapped);
    }

    /// Handles payload resolve.
    pub async fn handle_resolve(
        &self,
        payload_id: PayloadId,
        kind: PayloadKind,
        tx: tokio::sync::oneshot::Sender<Option<ResolveFuture>>,
    ) {
        let selected_basic = self.basic_selected_for_payload(payload_id).await;
        let (handle, health, builder) = if selected_basic {
            (self.basic_handle.clone(), self.basic_health.clone(), BASIC_BUILDER)
        } else {
            (self.flashblocks_handle.clone(), self.flashblocks_health.clone(), FLASHBLOCKS_BUILDER)
        };
        let deadline = self.getpayload_deadline;
        let payload_routes = Arc::clone(&self.payload_routes);
        let future = async move {
            let started = Instant::now();
            let result = handle.resolve_kind(payload_id, kind).await;
            let elapsed = started.elapsed();
            let mut routes = payload_routes.lock().await;
            if let Some(position) = routes.iter().position(|(id, _)| *id == payload_id) {
                routes.remove(position);
            }
            drop(routes);

            Self::record_selected_getpayload_latency(elapsed.as_secs_f64());
            if elapsed > deadline {
                Self::inc_selected_deadline_miss();
            }

            match result {
                Some(Ok(payload)) => Ok(payload),
                Some(Err(PayloadBuilderError::ChannelClosed)) => {
                    health.mark_unavailable();
                    Self::set_service_health_metric(builder, false);
                    Err(Self::unavailable_error(builder))
                }
                Some(Err(err)) => Err(err),
                None => {
                    if !health.is_healthy() {
                        Err(Self::unavailable_error(builder))
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
        let mut flashblocks_events = match self.flashblocks_handle.subscribe().await {
            Ok(events) => events.receiver,
            Err(error) => {
                self.flashblocks_health.mark_unavailable();
                Self::set_service_health_metric(FLASHBLOCKS_BUILDER, false);
                error!(
                    builder = FLASHBLOCKS_BUILDER,
                    error = %error,
                    "failed to subscribe to payload builder events"
                );
                return;
            }
        };
        let mut basic_events = match self.basic_handle.subscribe().await {
            Ok(events) => events.receiver,
            Err(error) => {
                self.basic_health.mark_unavailable();
                Self::set_service_health_metric(BASIC_BUILDER, false);
                error!(
                    builder = BASIC_BUILDER,
                    error = %error,
                    "failed to subscribe to payload builder events"
                );
                return;
            }
        };
        let (events_tx, events_rx) = broadcast::channel(64);
        if tx.send(events_rx).is_err() {
            return;
        }

        let router = self.clone();
        tokio::spawn(async move {
            let mut flashblocks_closed = false;
            let mut basic_closed = false;
            while !flashblocks_closed || !basic_closed {
                tokio::select! {
                    _ = events_tx.closed() => break,
                    result = flashblocks_events.recv(), if !flashblocks_closed => match result {
                        Ok(event) => {
                            if !router.basic_selected_at(Self::event_timestamp(&event)) {
                                let _ = events_tx.send(event);
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            flashblocks_closed = true;
                            router.flashblocks_health.mark_unavailable();
                            Self::set_service_health_metric(FLASHBLOCKS_BUILDER, false);
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            metrics::counter!(
                                "mux_subscription_lagged_events_total",
                                "builder" => FLASHBLOCKS_BUILDER
                            )
                            .increment(skipped);
                            warn!(
                                builder = FLASHBLOCKS_BUILDER,
                                skipped,
                                "payload builder event forwarding lagged"
                            );
                        }
                    },
                    result = basic_events.recv(), if !basic_closed => match result {
                        Ok(event) => {
                            if router.basic_selected_at(Self::event_timestamp(&event)) {
                                let _ = events_tx.send(event);
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            basic_closed = true;
                            router.basic_health.mark_unavailable();
                            Self::set_service_health_metric(BASIC_BUILDER, false);
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            metrics::counter!(
                                "mux_subscription_lagged_events_total",
                                "builder" => BASIC_BUILDER
                            )
                            .increment(skipped);
                            warn!(
                                builder = BASIC_BUILDER,
                                skipped,
                                "payload builder event forwarding lagged"
                            );
                        }
                    },
                }
            }
        });
    }

    /// Maps selected-builder read results with unavailable conversion.
    pub fn map_read_result<T>(
        &self,
        result: Option<Result<T, PayloadBuilderError>>,
        builder: &'static str,
        health: &HealthState,
    ) -> Option<Result<T, PayloadBuilderError>> {
        match result {
            Some(Err(PayloadBuilderError::ChannelClosed)) => {
                health.mark_unavailable();
                Self::set_service_health_metric(builder, false);
                Some(Err(Self::unavailable_error(builder)))
            }
            None if !health.is_healthy() => Some(Err(Self::unavailable_error(builder))),
            other => other,
        }
    }

    /// Creates unavailable error.
    pub fn unavailable_error(builder: &'static str) -> PayloadBuilderError {
        PayloadBuilderError::other(BuilderUnavailableError { builder })
    }

    /// Increments dispatch metric.
    pub fn inc_dispatch_metric(builder: &'static str) {
        metrics::counter!("mux_builds_dispatched_total", "builder" => builder).increment(1);
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
    use std::time::Duration;

    use alloy_primitives::B256;
    use base_common_genesis::BaseUpgrade;
    use base_execution_chainspec::BaseChainSpecBuilder;
    use reth_ethereum_forks::ForkCondition;
    use reth_payload_builder::PayloadBuilderHandle;
    use reth_payload_builder_primitives::Events;
    use tokio::sync::{broadcast, mpsc};

    use super::*;

    const DENIM_TIMESTAMP: u64 = 10;

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
            Arc::new(
                BaseChainSpecBuilder::base_mainnet()
                    .with_fork(BaseUpgrade::Denim, ForkCondition::Timestamp(DENIM_TIMESTAMP))
                    .build(),
            ),
            RoutingConfig::default(),
        );
        (router, flash_rx, basic_rx)
    }

    fn sample_input(
        timestamp: u64,
    ) -> BuildNewPayload<<BaseEngineTypes as PayloadTypes>::PayloadAttributes> {
        let mut input = BuildNewPayload {
            attributes: base_execution_payload_builder::BasePayloadBuilderAttributes::default(),
            parent_hash: B256::ZERO,
            resources: Default::default(),
        };
        input.attributes.payload_attributes.timestamp = timestamp;
        input
    }

    fn payload_id_from_byte(value: u8) -> PayloadId {
        let mut input = sample_input(0);
        input.parent_hash = B256::repeat_byte(value);
        input.payload_id()
    }

    #[tokio::test]
    async fn build_fans_to_both_and_selects_by_denim_activation() {
        for (timestamp, selected_basic) in [(DENIM_TIMESTAMP - 1, false), (DENIM_TIMESTAMP, true)] {
            let (router, mut flash_rx, mut basic_rx) = test_router();
            let (tx, rx) = tokio::sync::oneshot::channel();

            let input = sample_input(timestamp);
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
                assert_eq!(input.payload_id() == payload_id, !selected_basic);
                assert_eq!(input.attributes.no_tx_pool, selected_basic);
                assert!(input.resources.execution_cache().is_none());
                assert!(input.resources.state_root_handle().is_none());
                tx.send(if selected_basic {
                    Err(PayloadBuilderError::MissingPayload)
                } else {
                    Ok(payload_id)
                })
                .expect("flash response");
            }

            if let PayloadServiceCommand::BuildNewPayload(input, _, tx) = basic_cmd {
                basic_seen = true;
                assert_eq!(input.payload_id() == payload_id, selected_basic);
                assert_eq!(input.attributes.no_tx_pool, !selected_basic);
                tx.send(if selected_basic {
                    Ok(payload_id)
                } else {
                    Err(PayloadBuilderError::MissingPayload)
                })
                .expect("basic response");
            }

            assert!(flash_seen);
            assert!(basic_seen);
            assert!(rx.await.expect("selected response").is_ok());
        }
    }

    #[tokio::test]
    async fn best_payload_reads_recorded_builder() {
        let (router, mut flash_rx, mut basic_rx) = test_router();
        let payload_id = payload_id_from_byte(7);
        router.record_payload_route(payload_id, true).await;
        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            router.handle_best_payload(payload_id, tx).await;
        });

        let basic_cmd = basic_rx.recv().await.expect("basic command");
        assert!(flash_rx.try_recv().is_err());
        if let PayloadServiceCommand::BestPayload(inner_payload_id, tx) = basic_cmd {
            assert_eq!(inner_payload_id, payload_id);
            tx.send(None).expect("send basic response");
        } else {
            panic!("expected BestPayload command");
        }

        assert!(rx.await.expect("best response").is_none());
    }

    #[tokio::test]
    async fn payload_timestamp_defaults_unknown_payload_to_flashblocks() {
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
    async fn subscribe_forwards_events_from_builder_selected_at_event_timestamp() {
        let (router, mut flash_rx, mut basic_rx) = test_router();
        let (sub_tx, sub_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            router.handle_subscribe(sub_tx).await;
        });

        let (flash_events_tx, flash_events_rx) = broadcast::channel(2);
        let (basic_events_tx, basic_events_rx) = broadcast::channel(2);
        let flash_cmd = flash_rx.recv().await.expect("flash subscribe command");
        let PayloadServiceCommand::Subscribe(flash_tx) = flash_cmd else {
            panic!("expected flash Subscribe command");
        };
        flash_tx.send(flash_events_rx).expect("send flash receiver");

        let basic_cmd = basic_rx.recv().await.expect("basic subscribe command");
        let PayloadServiceCommand::Subscribe(basic_tx) = basic_cmd else {
            panic!("expected basic Subscribe command");
        };
        basic_tx.send(basic_events_rx).expect("send basic receiver");
        let mut sub = sub_rx.await.expect("outer subscribe receiver");

        flash_events_tx
            .send(Events::Attributes(sample_input(DENIM_TIMESTAMP - 1).attributes))
            .expect("send pre-Denim flash event");
        assert_eq!(
            MultiplexRouter::event_timestamp(&sub.recv().await.expect("receive flash event")),
            DENIM_TIMESTAMP - 1
        );

        basic_events_tx
            .send(Events::Attributes(sample_input(DENIM_TIMESTAMP).attributes))
            .expect("send post-Denim basic event");
        assert_eq!(
            MultiplexRouter::event_timestamp(&sub.recv().await.expect("receive basic event")),
            DENIM_TIMESTAMP
        );

        drop(sub);
        tokio::time::timeout(Duration::from_secs(1), flash_events_tx.closed())
            .await
            .expect("forwarding task should unsubscribe when downstream receiver closes");
        assert_eq!(basic_events_tx.receiver_count(), 0);
    }

    #[tokio::test]
    async fn resolve_uses_recorded_builder() {
        let (router, mut flash_rx, mut basic_rx) = test_router();
        let payload_id = payload_id_from_byte(11);
        router.record_payload_route(payload_id, true).await;
        let (resolve_tx, resolve_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            router.handle_resolve(payload_id, PayloadKind::Earliest, resolve_tx).await;
        });

        let future =
            resolve_rx.await.expect("resolve response").expect("resolve future should be present");

        let resolve_task = tokio::spawn(future);

        let basic_cmd = basic_rx.recv().await.expect("basic resolve command");
        assert!(flash_rx.try_recv().is_err());
        if let PayloadServiceCommand::Resolve(inner_payload_id, _, tx) = basic_cmd {
            assert_eq!(inner_payload_id, payload_id);
            assert!(tx.send(None).is_ok(), "send basic resolve response");
        } else {
            panic!("expected Resolve command");
        }

        let resolved = resolve_task.await.expect("resolve task join");
        assert!(matches!(resolved, Err(PayloadBuilderError::MissingPayload)));
    }

    #[tokio::test]
    async fn payload_routes_evict_oldest_abandoned_payload() {
        let (router, _, _) = test_router();
        for value in 0..=MAX_PAYLOAD_ROUTES {
            router.record_payload_route(payload_id_from_byte(value as u8), true).await;
        }

        assert_eq!(router.payload_routes.lock().await.len(), MAX_PAYLOAD_ROUTES);
        assert!(!router.basic_selected_for_payload(payload_id_from_byte(0)).await);
        assert!(
            router.basic_selected_for_payload(payload_id_from_byte(MAX_PAYLOAD_ROUTES as u8)).await
        );
    }
}
