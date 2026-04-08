//! Balance monitor [`ProviderLayer`] implementation.
//!
//! Provides a [`BalanceMonitorLayer`] that plugs into any alloy provider stack
//! via [`ProviderBuilder::layer`]. When the layer is applied it spawns a
//! background task that polls the monitored address's balance on every new
//! block (via [`Provider::watch_blocks`]) and publishes the latest value
//! through a [`tokio::sync::watch`] channel.
//!
//! [`ProviderBuilder::layer`]: alloy_provider::ProviderBuilder::layer

use std::marker::PhantomData;

use alloy_network::Network;
use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, ProviderLayer, RootProvider};
use futures::StreamExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// A [`ProviderLayer`] that spawns a background balance-monitoring task.
///
/// The layer reuses the wrapped provider's existing transport — no extra HTTP
/// connection is needed. Balance updates are emitted on a
/// [`watch::Receiver<U256>`] obtained at construction time.
///
/// # Example
///
/// ```rust,ignore
/// let (layer, balance_rx) = BalanceMonitorLayer::new(address, cancel.clone());
///
/// let provider = ProviderBuilder::new()
///     .layer(layer)
///     .connect_http(rpc_url);
///
/// // Consume balance updates (e.g. for Prometheus metrics).
/// tokio::spawn(async move {
///     while balance_rx.changed().await.is_ok() {
///         let bal = *balance_rx.borrow_and_update();
///         metrics.set(f64::from(bal));
///     }
/// });
/// ```
#[derive(Debug)]
pub struct BalanceMonitorLayer {
    address: Address,
    cancel: CancellationToken,
    tx: watch::Sender<U256>,
}

impl BalanceMonitorLayer {
    /// Creates a new layer together with the receiving end of the balance
    /// channel.
    ///
    /// The returned [`watch::Receiver`] yields the latest observed balance
    /// (as [`U256`] in wei). The initial value is [`U256::ZERO`].
    pub fn new(address: Address, cancel: CancellationToken) -> (Self, watch::Receiver<U256>) {
        let (tx, rx) = watch::channel(U256::ZERO);
        (Self { address, cancel, tx }, rx)
    }
}

impl<P, N> ProviderLayer<P, N> for BalanceMonitorLayer
where
    P: Provider<N> + Clone + 'static,
    N: Network,
{
    type Provider = BalanceMonitorProvider<P, N>;

    fn layer(&self, inner: P) -> Self::Provider {
        let provider = inner.clone();
        let address = self.address;
        let cancel = self.cancel.clone();
        let tx = self.tx.clone();

        tokio::spawn(async move {
            let poller = match provider.watch_blocks().await {
                Ok(poller) => poller,
                Err(e) => {
                    warn!(error = %e, address = %address, "failed to install block filter, balance monitor disabled");
                    return;
                }
            };
            let mut stream = poller.into_stream().flat_map(futures::stream::iter);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    block = stream.next() => {
                        if block.is_none() {
                            break;
                        }
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

        BalanceMonitorProvider { inner, _network: PhantomData }
    }
}

/// Transparent pass-through provider created by [`BalanceMonitorLayer`].
///
/// This provider delegates every call to the inner provider unchanged.
/// The balance monitoring happens entirely in a background task spawned by
/// the layer.
#[derive(Clone, Debug)]
pub struct BalanceMonitorProvider<P, N = alloy_network::Ethereum> {
    inner: P,
    _network: PhantomData<N>,
}

impl<P: Provider<N>, N: Network> Provider<N> for BalanceMonitorProvider<P, N> {
    fn root(&self) -> &RootProvider<N> {
        self.inner.root()
    }
}
