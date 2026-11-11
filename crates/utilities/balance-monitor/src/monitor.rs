//! Balance monitor [`ProviderLayer`] implementation.
//!
//! Provides a [`BalanceMonitorLayer`] that plugs into any alloy provider stack
//! via [`ProviderBuilder::layer`]. When the layer is applied it spawns a
//! background task that polls the monitored address's balance immediately,
//! then every configured interval. Successful reads publish through a
//! [`tokio::sync::watch`] channel; failed reads only log a warning. Cancelling
//! the token stops the task, abandoning any balance read in flight.
//!
//! The layer is an identity layer — it returns the inner provider unchanged,
//! so the resulting provider type is the same as if the layer were never
//! applied. This allows callers to conditionally apply the layer without
//! introducing type divergence.
//!
//! [`ProviderBuilder::layer`]: alloy_provider::ProviderBuilder::layer

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use alloy_network::Network;
use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, ProviderLayer};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// An identity [`ProviderLayer`] that spawns a background balance-monitoring
/// task.
///
/// The layer reuses the wrapped provider's existing transport — no extra HTTP
/// connection is needed. Balance updates are emitted on a
/// [`watch::Receiver<U256>`] obtained at construction time.
///
/// Because this is an identity layer (`type Provider = P`), the resulting
/// provider type is unchanged. Callers that do not need balance monitoring
/// should simply not apply this layer to the provider stack.
///
/// Each instance must only be layered once. A second call to
/// [`ProviderLayer::layer`] will log a warning and skip spawning a
/// duplicate task.
///
/// # Example
///
/// ```rust,ignore
/// let (layer, balance_rx) = BalanceMonitorLayer::new(
///     address,
///     cancel.clone(),
///     BalanceMonitorLayer::DEFAULT_POLL_INTERVAL,
/// );
///
/// let provider = ProviderBuilder::new()
///     .layer(layer)
///     .connect_http(rpc_url);
///
/// tokio::spawn(async move {
///     let mut rx = balance_rx;
///     while rx.changed().await.is_ok() {
///         metrics.set(f64::from(*rx.borrow_and_update()));
///     }
/// });
/// ```
#[derive(Debug)]
pub struct BalanceMonitorLayer {
    address: Address,
    cancel: CancellationToken,
    poll_interval: Duration,
    tx: watch::Sender<U256>,
    spawned: AtomicBool,
}

impl BalanceMonitorLayer {
    /// Default polling interval (12 seconds, matching L1 block time).
    pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(12);

    /// Shortest polling interval; shorter intervals, including zero, are raised to it.
    pub const MIN_POLL_INTERVAL: Duration = Duration::from_millis(1);

    /// Creates a layer that polls `address` immediately when applied, then every `poll_interval`.
    ///
    /// A `poll_interval` below [`Self::MIN_POLL_INTERVAL`] is raised to it.
    ///
    /// The returned [`watch::Receiver`] yields the latest observed balance
    /// (as [`U256`] in wei). The initial [`U256::ZERO`] is a placeholder, not an
    /// observed balance. Wait for [`watch::Receiver::changed`] before reading it.
    /// Every successful read notifies receivers, even for zero or unchanged
    /// balances. Failed reads log a warning without publishing an update.
    pub fn new(
        address: Address,
        cancel: CancellationToken,
        poll_interval: Duration,
    ) -> (Self, watch::Receiver<U256>) {
        let (tx, rx) = watch::channel(U256::ZERO);
        let poll_interval = poll_interval.max(Self::MIN_POLL_INTERVAL);
        (Self { address, cancel, poll_interval, tx, spawned: AtomicBool::new(false) }, rx)
    }
}

impl<P, N> ProviderLayer<P, N> for BalanceMonitorLayer
where
    P: Provider<N> + Clone + Send + 'static,
    N: Network,
{
    type Provider = P;

    fn layer(&self, inner: P) -> P {
        if self.spawned.swap(true, Ordering::Relaxed) {
            warn!(address = %self.address, "balance monitor already spawned, skipping duplicate");
            return inner;
        }

        let provider = inner.clone();
        let address = self.address;
        let cancel = self.cancel.clone();
        let poll_interval = self.poll_interval;
        let tx = self.tx.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(poll_interval);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = interval.tick() => {}
                }
                let balance = tokio::select! {
                    () = cancel.cancelled() => break,
                    balance = provider.get_balance(address) => balance,
                };
                match balance {
                    Ok(bal) => {
                        let _ = tx.send(bal);
                        debug!(balance = %bal, address = %address, "recorded account balance");
                    }
                    Err(e) => {
                        warn!(error = %e, address = %address, "failed to fetch account balance");
                    }
                }
            }
        });

        inner
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_primitives::{Address, U256};
    use alloy_provider::ProviderBuilder;
    use tokio_util::sync::CancellationToken;

    use super::BalanceMonitorLayer;

    #[tokio::test]
    async fn publishes_balance_before_default_poll_interval() {
        let anvil = alloy_node_bindings::Anvil::new().spawn();
        let address: Address = anvil.addresses()[0];
        let cancel = CancellationToken::new();

        let (layer, mut balance_rx) = BalanceMonitorLayer::new(
            address,
            cancel.clone(),
            BalanceMonitorLayer::DEFAULT_POLL_INTERVAL,
        );
        let _provider = ProviderBuilder::new().layer(layer).connect_http(anvil.endpoint_url());

        let update = tokio::time::timeout(Duration::from_secs(3), balance_rx.changed()).await;
        cancel.cancel();
        update
            .expect("first balance read must not wait for the polling interval")
            .expect("watch channel closed unexpectedly");

        let balance = *balance_rx.borrow();
        assert!(balance > U256::ZERO, "balance should be positive, got {balance}");
    }

    #[tokio::test]
    async fn publishes_zero_balance_on_first_read() {
        let anvil = alloy_node_bindings::Anvil::new().spawn();
        let cancel = CancellationToken::new();
        let (layer, mut balance_rx) = BalanceMonitorLayer::new(
            Address::ZERO,
            cancel.clone(),
            BalanceMonitorLayer::DEFAULT_POLL_INTERVAL,
        );
        let _provider = ProviderBuilder::new().layer(layer).connect_http(anvil.endpoint_url());

        let update = tokio::time::timeout(Duration::from_secs(3), balance_rx.changed()).await;
        cancel.cancel();
        update
            .expect("a real zero balance must notify the receiver without waiting for the interval")
            .expect("watch channel closed unexpectedly");
        assert_eq!(*balance_rx.borrow(), U256::ZERO);
    }

    #[tokio::test]
    async fn zero_poll_interval_keeps_publishing() {
        let anvil = alloy_node_bindings::Anvil::new().spawn();
        let cancel = CancellationToken::new();
        let (layer, mut balance_rx) =
            BalanceMonitorLayer::new(anvil.addresses()[0], cancel.clone(), Duration::ZERO);
        let _provider = ProviderBuilder::new().layer(layer).connect_http(anvil.endpoint_url());

        let updates = tokio::time::timeout(Duration::from_secs(3), async {
            balance_rx.changed().await?;
            balance_rx.changed().await
        })
        .await;
        cancel.cancel();
        updates
            .expect("a zero poll interval must still poll repeatedly")
            .expect("polling task must not stop on a zero poll interval");
    }

    #[tokio::test]
    async fn cancellation_interrupts_pending_balance_read() {
        let unresponsive_rpc =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind listener");
        let rpc_url = format!("http://{}", unresponsive_rpc.local_addr().expect("local addr"));
        let cancel = CancellationToken::new();
        let (layer, mut balance_rx) = BalanceMonitorLayer::new(
            Address::ZERO,
            cancel.clone(),
            BalanceMonitorLayer::DEFAULT_POLL_INTERVAL,
        );
        let _provider =
            ProviderBuilder::new().layer(layer).connect_http(rpc_url.parse().expect("valid url"));

        // Accept the balance request but never answer it.
        let _connection = tokio::time::timeout(Duration::from_secs(3), unresponsive_rpc.accept())
            .await
            .expect("the monitor must send a balance request")
            .expect("balance request connection");
        cancel.cancel();

        let closed = tokio::time::timeout(Duration::from_secs(3), balance_rx.changed()).await;
        assert!(
            matches!(closed, Ok(Err(_))),
            "cancellation must stop the task while a balance read is pending"
        );
    }

    #[tokio::test]
    async fn cancellation_closes_channel() {
        let anvil = alloy_node_bindings::Anvil::new().spawn();
        let address: Address = anvil.addresses()[0];
        let cancel = CancellationToken::new();

        let (layer, mut balance_rx) = BalanceMonitorLayer::new(
            address,
            cancel.clone(),
            BalanceMonitorLayer::DEFAULT_POLL_INTERVAL,
        );
        let _provider = ProviderBuilder::new().layer(layer).connect_http(anvil.endpoint_url());

        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if balance_rx.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
        assert!(result.is_ok(), "channel should close after cancellation");
    }
}
