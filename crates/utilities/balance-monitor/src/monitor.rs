//! Balance monitor [`ProviderLayer`] implementation.
//!
//! Provides a [`BalanceMonitorLayer`] that plugs into any alloy provider stack
//! via [`ProviderBuilder::layer`]. When the layer is applied it spawns a
//! background task that periodically polls the monitored address's balance
//! and publishes the latest value through a [`tokio::sync::watch`] channel.
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

    /// Creates a new layer that polls `address` every `poll_interval`.
    ///
    /// The returned [`watch::Receiver`] yields the latest observed balance
    /// (as [`U256`] in wei). The initial value is [`U256::ZERO`].
    pub fn new(
        address: Address,
        cancel: CancellationToken,
        poll_interval: Duration,
    ) -> (Self, watch::Receiver<U256>) {
        let (tx, rx) = watch::channel(U256::ZERO);
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
            interval.tick().await;
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = interval.tick() => {
                        match provider.get_balance(address).await {
                            Ok(bal) => {
                                let _ = tx.send(bal);
                                debug!(balance = %bal, address = %address, "recorded account balance");
                            }
                            Err(e) => {
                                warn!(error = %e, address = %address, "failed to fetch account balance");
                            }
                        }
                    }
                }
            }
        });

        inner
    }
}
