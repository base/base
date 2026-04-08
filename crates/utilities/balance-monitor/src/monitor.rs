//! Balance monitor [`ProviderLayer`] implementation.
//!
//! Provides a [`BalanceMonitorLayer`] that plugs into any alloy provider stack
//! via [`ProviderBuilder::layer`]. When the layer is applied it spawns a
//! background task that polls the monitored address's balance on every new
//! block (via [`Provider::watch_blocks`]) and publishes the latest value
//! through a [`tokio::sync::watch`] channel.
//!
//! The layer is an identity layer — it returns the inner provider unchanged,
//! so the resulting provider type is the same as if the layer were never
//! applied. This allows callers to conditionally apply the layer without
//! introducing type divergence.
//!
//! [`ProviderBuilder::layer`]: alloy_provider::ProviderBuilder::layer

use alloy_network::Network;
use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, ProviderLayer};
use futures::StreamExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

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
/// # Example
///
/// ```rust,ignore
/// let (layer, balance_rx) = BalanceMonitorLayer::new(address, cancel.clone());
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
    P: Provider<N> + Clone + Send + 'static,
    N: Network,
{
    type Provider = P;

    fn layer(&self, inner: P) -> P {
        let provider = inner.clone();
        let address = self.address;
        let cancel = self.cancel.clone();
        let tx = self.tx.clone();

        tokio::spawn(async move {
            let poller = match provider.watch_blocks().await {
                Ok(poller) => poller,
                Err(e) => {
                    error!(error = %e, address = %address, "failed to install block filter, balance monitor disabled");
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

        inner
    }
}
