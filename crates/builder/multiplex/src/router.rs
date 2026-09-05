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
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use reth_payload_builder::{BuildNewPayload, PayloadBuilderHandle, PayloadServiceCommand};
use reth_payload_builder_primitives::{Events, PayloadBuilderError};
use reth_payload_primitives::{PayloadAttributes, PayloadKind, PayloadTypes};
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

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

/// Router that cuts payload selection from flashblocks to basic after Beryl.
#[derive(Debug)]
pub struct MultiplexRouter {
    /// Flashblocks payload-builder handle.
    pub flashblocks_handle: PayloadBuilderHandle<BaseEngineTypes>,
    /// Basic payload-builder handle.
    pub basic_handle: PayloadBuilderHandle<BaseEngineTypes>,
    /// Flashblocks health state.
    pub flashblocks_health: HealthState,
    /// Basic health state.
    pub basic_health: HealthState,
    /// Chain spec that owns the upgrade activation conditions.
    pub chain_spec: Arc<BaseChainSpec>,
    /// Whether recent payload IDs are routed to the basic builder, ordered oldest first.
    pub payload_routes: VecDeque<(PayloadId, bool)>,
}

impl MultiplexRouter {
    /// Creates a new router.
    pub const fn new(
        flashblocks_handle: PayloadBuilderHandle<BaseEngineTypes>,
        basic_handle: PayloadBuilderHandle<BaseEngineTypes>,
        flashblocks_health: HealthState,
        basic_health: HealthState,
        chain_spec: Arc<BaseChainSpec>,
    ) -> Self {
        Self {
            flashblocks_handle,
            basic_handle,
            flashblocks_health,
            basic_health,
            chain_spec,
            payload_routes: VecDeque::new(),
        }
    }

    /// Returns whether a post-Beryl upgrade selects the basic builder at `timestamp`.
    pub fn basic_selected_at(&self, timestamp: u64) -> bool {
        self.chain_spec.is_post_beryl_active_at_timestamp(timestamp)
    }

    /// Returns the recorded route for a payload, defaulting unknown payloads to flashblocks.
    pub fn basic_selected_for_payload(&self, payload_id: PayloadId) -> bool {
        self.payload_routes
            .iter()
            .find_map(|(id, selected_basic)| (*id == payload_id).then_some(*selected_basic))
            .unwrap_or(false)
    }

    /// Records a payload route while bounding abandoned payload state.
    pub fn record_payload_route(&mut self, payload_id: PayloadId, selected_basic: bool) {
        if let Some((_, route)) = self.payload_routes.iter_mut().find(|(id, _)| *id == payload_id) {
            *route = selected_basic;
            return;
        }
        if self.payload_routes.len() == MAX_PAYLOAD_ROUTES {
            self.payload_routes.pop_front();
        }
        self.payload_routes.push_back((payload_id, selected_basic));
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
        mut self,
        mut rx: mpsc::UnboundedReceiver<PayloadServiceCommand<BaseEngineTypes>>,
    ) {
        let mut responses: FuturesUnordered<BoxFuture<'static, ()>> = FuturesUnordered::new();
        loop {
            // Poll newly queued response work before accepting another command so each inner
            // service observes commands in the same order as this router.
            tokio::select! {
                biased;
                Some(()) = responses.next(), if !responses.is_empty() => {}
                command = rx.recv() => match command {
                    Some(command) => {
                        if let Some(response) = self.handle_command(command) {
                            responses.push(response);
                        }
                    }
                    None => break,
                }
            }
        }
    }

    /// Handles a single command.
    pub fn handle_command(
        &mut self,
        command: PayloadServiceCommand<BaseEngineTypes>,
    ) -> Option<BoxFuture<'static, ()>> {
        match command {
            PayloadServiceCommand::BuildNewPayload(input, _span, tx) => {
                Some(self.handle_build_new_payload(*input, tx))
            }
            PayloadServiceCommand::BestPayload(payload_id, tx) => {
                Some(self.handle_best_payload(payload_id, tx))
            }
            PayloadServiceCommand::PayloadTimestamp(payload_id, tx) => {
                Some(self.handle_payload_timestamp(payload_id, tx))
            }
            PayloadServiceCommand::Resolve(payload_id, kind, tx) => {
                Some(self.handle_resolve(payload_id, kind, tx))
            }
            PayloadServiceCommand::Subscribe(tx) => Some(self.handle_subscribe(tx)),
        }
    }

    /// Handles build fan-out.
    pub fn handle_build_new_payload(
        &mut self,
        input: BuildNewPayload<<BaseEngineTypes as PayloadTypes>::PayloadAttributes>,
        tx: tokio::sync::oneshot::Sender<Result<PayloadId, PayloadBuilderError>>,
    ) -> BoxFuture<'static, ()> {
        let payload_id = input.payload_id();
        let selected_basic = self.basic_selected_at(input.attributes.timestamp());
        self.record_payload_route(payload_id, selected_basic);

        if selected_basic {
            Self::inc_dispatch_metric(BASIC_BUILDER);
            Self::inc_selected_build_metric(BASIC_BUILDER);
            let basic_rx = self.basic_handle.send_new_payload(input);
            let basic_health = self.basic_health.clone();
            return async move {
                let result = basic_rx.await.unwrap_or_else(|_| {
                    basic_health.mark_unavailable();
                    Self::set_service_health_metric(BASIC_BUILDER, false);
                    Err(Self::unavailable_error(BASIC_BUILDER))
                });
                info!(
                    builder = BASIC_BUILDER,
                    payload_id = ?payload_id,
                    selected = true,
                    result = if result.is_ok() { "ok" } else { "err" },
                    "multiplex build request completed"
                );
                let _ = tx.send(result);
            }
            .boxed();
        }

        let mut shadow_attributes = input.attributes.clone();
        shadow_attributes.no_tx_pool = true;
        let shadow_input = BuildNewPayload {
            attributes: shadow_attributes,
            parent_hash: input.parent_hash,
            resources: Default::default(),
        };

        Self::inc_dispatch_metric(FLASHBLOCKS_BUILDER);
        Self::inc_dispatch_metric(BASIC_BUILDER);

        let selected_rx = self.flashblocks_handle.send_new_payload(input);
        let selected_health = self.flashblocks_health.clone();
        let selected_builder = FLASHBLOCKS_BUILDER;
        let shadow_rx = self.basic_handle.send_new_payload(shadow_input);
        let shadow_health = self.basic_health.clone();
        let shadow_builder = BASIC_BUILDER;

        Self::inc_selected_build_metric(selected_builder);
        async move {
            let selected_response = async move {
                let selected_result = selected_rx.await.unwrap_or_else(|_| {
                    selected_health.mark_unavailable();
                    Self::set_service_health_metric(selected_builder, false);
                    Err(Self::unavailable_error(selected_builder))
                });
                info!(
                    builder = selected_builder,
                    payload_id = ?payload_id,
                    selected = true,
                    result = if selected_result.is_ok() { "ok" } else { "err" },
                    "multiplex build request completed"
                );
                let _ = tx.send(selected_result);
            };
            let shadow_response = async move {
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
            };
            futures::join!(selected_response, shadow_response);
        }
        .boxed()
    }

    /// Handles best payload lookup.
    pub fn handle_best_payload(
        &self,
        payload_id: PayloadId,
        tx: tokio::sync::oneshot::Sender<
            Option<Result<<BaseEngineTypes as PayloadTypes>::BuiltPayload, PayloadBuilderError>>,
        >,
    ) -> BoxFuture<'static, ()> {
        let selected_basic = self.basic_selected_for_payload(payload_id);
        let (handle, builder, health) = if selected_basic {
            (self.basic_handle.clone(), BASIC_BUILDER, self.basic_health.clone())
        } else {
            (self.flashblocks_handle.clone(), FLASHBLOCKS_BUILDER, self.flashblocks_health.clone())
        };
        async move {
            let result = handle.best_payload(payload_id).await;
            let mapped = Self::map_read_result(result, builder, &health);
            let _ = tx.send(mapped);
        }
        .boxed()
    }

    /// Handles payload timestamp lookup.
    pub fn handle_payload_timestamp(
        &self,
        payload_id: PayloadId,
        tx: tokio::sync::oneshot::Sender<Option<Result<u64, PayloadBuilderError>>>,
    ) -> BoxFuture<'static, ()> {
        let selected_basic = self.basic_selected_for_payload(payload_id);
        let (handle, builder, health) = if selected_basic {
            (self.basic_handle.clone(), BASIC_BUILDER, self.basic_health.clone())
        } else {
            (self.flashblocks_handle.clone(), FLASHBLOCKS_BUILDER, self.flashblocks_health.clone())
        };
        async move {
            let result = handle.payload_timestamp(payload_id).await;
            let mapped = Self::map_read_result(result, builder, &health);
            let _ = tx.send(mapped);
        }
        .boxed()
    }

    /// Handles payload resolve.
    pub fn handle_resolve(
        &self,
        payload_id: PayloadId,
        kind: PayloadKind,
        mut tx: tokio::sync::oneshot::Sender<Option<ResolveFuture>>,
    ) -> BoxFuture<'static, ()> {
        let selected_basic = self.basic_selected_for_payload(payload_id);
        let (handle, health, builder) = if selected_basic {
            (self.basic_handle.clone(), self.basic_health.clone(), BASIC_BUILDER)
        } else {
            (self.flashblocks_handle.clone(), self.flashblocks_health.clone(), FLASHBLOCKS_BUILDER)
        };
        async move {
            let started = Instant::now();
            let resolve = handle.resolve_kind(payload_id, kind);
            tokio::pin!(resolve);
            let result = tokio::select! {
                result = &mut resolve => result,
                _ = tx.closed() => return,
            };
            let elapsed = started.elapsed();
            Self::record_selected_getpayload_latency(elapsed.as_secs_f64());

            let result = match result {
                Some(Ok(payload)) => Some(Ok(payload)),
                Some(Err(PayloadBuilderError::ChannelClosed)) => {
                    health.mark_unavailable();
                    Self::set_service_health_metric(builder, false);
                    Some(Err(Self::unavailable_error(builder)))
                }
                Some(Err(err)) => Some(Err(err)),
                None if !health.is_healthy() => Some(Err(Self::unavailable_error(builder))),
                None => None,
            };

            let response = result.map(|result| Box::pin(async move { result }) as ResolveFuture);
            let _ = tx.send(response);
        }
        .boxed()
    }

    /// Handles subscriptions.
    pub fn handle_subscribe(
        &self,
        tx: tokio::sync::oneshot::Sender<
            tokio::sync::broadcast::Receiver<
                reth_payload_builder_primitives::Events<BaseEngineTypes>,
            >,
        >,
    ) -> BoxFuture<'static, ()> {
        let flashblocks_handle = self.flashblocks_handle.clone();
        let basic_handle = self.basic_handle.clone();
        let flashblocks_health = self.flashblocks_health.clone();
        let basic_health = self.basic_health.clone();
        let chain_spec = Arc::clone(&self.chain_spec);
        async move {
            let mut flashblocks_events = match flashblocks_handle.subscribe().await {
                Ok(events) => events.receiver,
                Err(error) => {
                    flashblocks_health.mark_unavailable();
                    Self::set_service_health_metric(FLASHBLOCKS_BUILDER, false);
                    error!(
                        builder = FLASHBLOCKS_BUILDER,
                        error = %error,
                        "failed to subscribe to payload builder events"
                    );
                    return;
                }
            };
            let mut basic_events = match basic_handle.subscribe().await {
                Ok(events) => events.receiver,
                Err(error) => {
                    basic_health.mark_unavailable();
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

            let mut flashblocks_closed = false;
            let mut basic_closed = false;
            while !flashblocks_closed || !basic_closed {
                tokio::select! {
                    _ = events_tx.closed() => break,
                    result = flashblocks_events.recv(), if !flashblocks_closed => match result {
                        Ok(event) => {
                            if !chain_spec.is_post_beryl_active_at_timestamp(Self::event_timestamp(&event)) {
                                let _ = events_tx.send(event);
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            flashblocks_closed = true;
                            flashblocks_health.mark_unavailable();
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
                            if chain_spec.is_post_beryl_active_at_timestamp(Self::event_timestamp(&event)) {
                                let _ = events_tx.send(event);
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            basic_closed = true;
                            basic_health.mark_unavailable();
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
        }
        .boxed()
    }

    /// Maps selected-builder read results with unavailable conversion.
    pub fn map_read_result<T>(
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
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_primitives::B256;
    use base_common_genesis::{BaseUpgrade, RuntimeUpgradeRegistry};
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
    async fn build_fans_out_before_post_beryl_activation_and_uses_only_basic_after() {
        for (timestamp, selected_basic) in [(DENIM_TIMESTAMP - 1, false), (DENIM_TIMESTAMP, true)] {
            let (mut router, mut flash_rx, mut basic_rx) = test_router();
            let (tx, rx) = tokio::sync::oneshot::channel();

            let input = sample_input(timestamp);
            let payload_id = input.payload_id();
            let response = router.handle_build_new_payload(input, tx);

            let basic_cmd = basic_rx.recv().await.expect("basic cmd");
            if !selected_basic {
                let flash_cmd = flash_rx.recv().await.expect("flash cmd");
                let PayloadServiceCommand::BuildNewPayload(input, _, tx) = flash_cmd else {
                    panic!("expected flash BuildNewPayload");
                };
                assert_eq!(input.payload_id() == payload_id, !selected_basic);
                assert_eq!(input.attributes.no_tx_pool, selected_basic);
                assert!(input.resources.execution_cache().is_none());
                assert!(input.resources.state_root_handle().is_none());
                tx.send(Ok(payload_id)).expect("flash response");
            } else {
                assert!(flash_rx.try_recv().is_err());
            }

            let PayloadServiceCommand::BuildNewPayload(input, _, tx) = basic_cmd else {
                panic!("expected basic BuildNewPayload");
            };
            assert_eq!(input.payload_id() == payload_id, selected_basic);
            assert_eq!(input.attributes.no_tx_pool, !selected_basic);
            tx.send(if selected_basic {
                Ok(payload_id)
            } else {
                Err(PayloadBuilderError::MissingPayload)
            })
            .expect("basic response");

            response.await;
            assert!(rx.await.expect("selected response").is_ok());
        }
    }

    #[test]
    fn beryl_is_eligible_until_a_later_upgrade_boundary() {
        let (mut router, _, _) = test_router();
        router.chain_spec = Arc::new(
            BaseChainSpecBuilder::base_mainnet()
                .with_fork(BaseUpgrade::Beryl, ForkCondition::Timestamp(1))
                .with_fork(BaseUpgrade::Cobalt, ForkCondition::Timestamp(10))
                .with_fork(BaseUpgrade::Denim, ForkCondition::Never)
                .with_fork(BaseUpgrade::Zenith, ForkCondition::Never)
                .build(),
        );

        assert!(!router.basic_selected_at(9));
        assert!(router.basic_selected_at(10));
        assert!(router.basic_selected_at(11));
    }

    #[test]
    fn runtime_post_beryl_activation_updates_existing_router() {
        const CHAIN_ID: u64 = 9_777_101;
        RuntimeUpgradeRegistry::clear_chain(CHAIN_ID);
        let (mut router, _, _) = test_router();
        router.chain_spec = Arc::new(
            BaseChainSpecBuilder::base_mainnet()
                .chain(CHAIN_ID.into())
                .with_fork(BaseUpgrade::Cobalt, ForkCondition::Never)
                .build(),
        );
        assert!(!router.basic_selected_at(10));

        RuntimeUpgradeRegistry::set_activation_timestamp(CHAIN_ID, BaseUpgrade::Cobalt, 10);
        assert!(router.basic_selected_at(10));
        RuntimeUpgradeRegistry::clear_chain(CHAIN_ID);
    }

    #[tokio::test]
    async fn post_beryl_builds_never_dispatch_to_flashblocks() {
        for upgrade in [BaseUpgrade::Cobalt, BaseUpgrade::Denim, BaseUpgrade::Zenith] {
            let (mut router, mut flash_rx, mut basic_rx) = test_router();
            router.chain_spec = Arc::new(
                BaseChainSpecBuilder::base_mainnet()
                    .with_fork(BaseUpgrade::Cobalt, ForkCondition::Never)
                    .with_fork(BaseUpgrade::Denim, ForkCondition::Never)
                    .with_fork(BaseUpgrade::Zenith, ForkCondition::Never)
                    .with_fork(upgrade, ForkCondition::Timestamp(10))
                    .build(),
            );
            let input = sample_input(10);
            let payload_id = input.payload_id();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let response = router.handle_build_new_payload(input, tx);

            assert!(flash_rx.try_recv().is_err(), "Flashblocks received work at {upgrade:?}");
            let PayloadServiceCommand::BuildNewPayload(input, _, tx) =
                basic_rx.try_recv().expect("native build command")
            else {
                panic!("expected native BuildNewPayload");
            };
            assert_eq!(input.payload_id(), payload_id);
            assert!(!input.attributes.no_tx_pool);
            tx.send(Ok(payload_id)).expect("native response");
            response.await;
            assert_eq!(rx.await.expect("selected response").expect("successful build"), payload_id);
        }
    }

    #[tokio::test]
    async fn post_beryl_native_error_does_not_fall_back_to_flashblocks() {
        let (mut router, mut flash_rx, mut basic_rx) = test_router();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let response = router.handle_build_new_payload(sample_input(DENIM_TIMESTAMP), tx);

        let PayloadServiceCommand::BuildNewPayload(_, _, tx) =
            basic_rx.recv().await.expect("native build command")
        else {
            panic!("expected native BuildNewPayload");
        };
        tx.send(Err(PayloadBuilderError::MissingPayload)).expect("native response");
        response.await;

        assert!(matches!(
            rx.await.expect("selected response"),
            Err(PayloadBuilderError::MissingPayload)
        ));
        assert!(flash_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn best_payload_reads_recorded_builder() {
        let (mut router, mut flash_rx, mut basic_rx) = test_router();
        let payload_id = payload_id_from_byte(7);
        router.record_payload_route(payload_id, true);
        let (tx, rx) = tokio::sync::oneshot::channel();

        let response = router.handle_best_payload(payload_id, tx);
        let inner = async {
            let basic_cmd = basic_rx.recv().await.expect("basic command");
            assert!(flash_rx.try_recv().is_err());
            if let PayloadServiceCommand::BestPayload(inner_payload_id, tx) = basic_cmd {
                assert_eq!(inner_payload_id, payload_id);
                tx.send(None).expect("send basic response");
            } else {
                panic!("expected BestPayload command");
            }
        };
        tokio::join!(response, inner);

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
            .send(Events::Attributes(sample_input(DENIM_TIMESTAMP - 1).attributes))
            .expect("send rejected pre-Denim basic event");
        flash_events_tx
            .send(Events::Attributes(sample_input(DENIM_TIMESTAMP).attributes))
            .expect("send rejected post-Denim flash event");
        assert!(tokio::time::timeout(Duration::from_millis(50), sub.recv()).await.is_err());

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
    async fn run_preserves_build_then_resolve_fifo_order() {
        let (router, mut flash_rx, mut basic_rx) = test_router();
        let (router_tx, router_rx) = mpsc::unbounded_channel();
        let handle = PayloadBuilderHandle::new(router_tx);
        let router_task = tokio::spawn(router.run(router_rx));
        let input = sample_input(DENIM_TIMESTAMP);
        let payload_id = input.payload_id();

        let build_rx = handle.send_new_payload(input);
        let resolve = handle.resolve_kind(payload_id, PayloadKind::Earliest);

        let basic_cmd = basic_rx.recv().await.expect("basic build command");
        let PayloadServiceCommand::BuildNewPayload(_, _, basic_build_tx) = basic_cmd else {
            panic!("expected basic BuildNewPayload command");
        };
        let basic_resolve = tokio::time::timeout(Duration::from_secs(1), basic_rx.recv())
            .await
            .expect("resolve should not wait for build responses")
            .expect("basic resolve command");
        let PayloadServiceCommand::Resolve(inner_payload_id, _, basic_resolve_tx) = basic_resolve
        else {
            panic!("expected basic Resolve command after BuildNewPayload");
        };
        assert_eq!(inner_payload_id, payload_id);
        assert!(flash_rx.try_recv().is_err());

        basic_build_tx.send(Ok(payload_id)).expect("basic build response");
        assert!(basic_resolve_tx.send(None).is_ok(), "basic resolve response");

        assert_eq!(
            build_rx.await.expect("selected build response").expect("successful build"),
            payload_id
        );
        assert!(resolve.await.is_none());

        drop(handle);
        router_task.await.expect("router task");
    }

    #[tokio::test]
    async fn stalled_shadow_does_not_block_selected_builder() {
        for timestamp in [DENIM_TIMESTAMP - 2, DENIM_TIMESTAMP - 1] {
            let (router, mut flash_rx, mut basic_rx) = test_router();
            let (router_tx, router_rx) = mpsc::unbounded_channel();
            let handle = PayloadBuilderHandle::new(router_tx);
            let router_task = tokio::spawn(router.run(router_rx));
            let input = sample_input(timestamp);
            let payload_id = input.payload_id();
            let build_rx = handle.send_new_payload(input);

            let flash_cmd = flash_rx.recv().await.expect("flash build command");
            let PayloadServiceCommand::BuildNewPayload(_, _, flash_tx) = flash_cmd else {
                panic!("expected flash BuildNewPayload command");
            };
            let basic_cmd = basic_rx.recv().await.expect("basic build command");
            let PayloadServiceCommand::BuildNewPayload(_, _, basic_tx) = basic_cmd else {
                panic!("expected basic BuildNewPayload command");
            };
            flash_tx.send(Ok(payload_id)).expect("selected build response");

            let result = tokio::time::timeout(Duration::from_secs(1), build_rx)
                .await
                .expect("selected response should not wait for shadow")
                .expect("selected response channel")
                .expect("successful selected response");
            assert_eq!(result, payload_id);

            drop(basic_tx);
            drop(handle);
            router_task.await.expect("router task");
        }
    }

    #[tokio::test]
    async fn repeated_resolve_uses_recorded_builder() {
        let (mut router, mut flash_rx, mut basic_rx) = test_router();
        let payload_id = payload_id_from_byte(11);
        router.record_payload_route(payload_id, true);

        for _ in 0..2 {
            let (resolve_tx, resolve_rx) = tokio::sync::oneshot::channel();
            let response = router.handle_resolve(payload_id, PayloadKind::Earliest, resolve_tx);
            let inner = async {
                let basic_cmd = basic_rx.recv().await.expect("basic resolve command");
                assert!(flash_rx.try_recv().is_err());
                if let PayloadServiceCommand::Resolve(inner_payload_id, _, tx) = basic_cmd {
                    assert_eq!(inner_payload_id, payload_id);
                    assert!(tx.send(None).is_ok(), "send basic resolve response");
                } else {
                    panic!("expected Resolve command");
                }
            };
            tokio::join!(response, inner);
            assert!(resolve_rx.await.expect("resolve response").is_none());
        }

        assert!(router.basic_selected_for_payload(payload_id));
    }

    #[tokio::test]
    async fn payload_routes_preserve_fifo_when_rerecorded() {
        let (mut router, _, _) = test_router();
        for value in 0..MAX_PAYLOAD_ROUTES {
            router.record_payload_route(payload_id_from_byte(value as u8), true);
        }
        router.record_payload_route(payload_id_from_byte(0), true);
        router.record_payload_route(payload_id_from_byte(MAX_PAYLOAD_ROUTES as u8), true);

        assert_eq!(router.payload_routes.len(), MAX_PAYLOAD_ROUTES);
        assert!(!router.basic_selected_for_payload(payload_id_from_byte(0)));
        assert!(router.basic_selected_for_payload(payload_id_from_byte(MAX_PAYLOAD_ROUTES as u8)));
    }
}
