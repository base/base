use alloy_transport::{TransportResult, impl_future};

use crate::{PubSubFrontend, handle::ConnectionHandle, service::PubSubService};

/// Configuration objects that contain connection details for a backend.
///
/// Implementers should contain configuration options for the underlying
/// transport.
#[auto_impl::auto_impl(Box)]
pub trait PubSubConnect: Sized + Send + Sync + 'static {
    /// Returns `true` if the transport connects to a local resource.
    fn is_local(&self) -> bool;

    /// Spawn the backend, returning a handle to it.
    ///
    /// This function MUST create a long-lived task containing a
    /// [`ConnectionInterface`], and return the corresponding handle.
    ///
    /// [`ConnectionInterface`]: crate::ConnectionInterface
    fn connect(&self) -> impl_future!(<Output = TransportResult<ConnectionHandle>>);

    /// Attempt to reconnect the transport.
    ///
    /// Override this to add custom reconnection logic to your connector. This
    /// will be used by the internal pubsub connection managers in the event the
    /// connection fails.
    fn try_reconnect(&self) -> impl_future!(<Output = TransportResult<ConnectionHandle>>) {
        self.connect()
    }

    /// Convert the configuration object into a service with a running backend.
    fn into_service(self) -> impl_future!(<Output = TransportResult<PubSubFrontend>>) {
        PubSubService::connect(self)
    }
}
