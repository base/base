//! Error handling and conversion for the Base `eth` API.

use crate::RpcNodeCore;

/// Errors and stateful conversion used by the Base `eth_` and adjacent endpoints.
///
/// Converters provide context such as deposit metadata and L1 receipt fees.
pub trait EthApiTypes: RpcNodeCore + Send + Sync + Clone {
    /// Returns reference to transaction response builder.
    fn converter(&self) -> &crate::BaseRpcConverter<Self::Provider>;
}

/// Helper trait holds necessary trait bounds on [`EthApiTypes`] to implement `eth` API.
pub trait FullEthApiTypes
where
    Self: RpcNodeCore + EthApiTypes,
{
}

impl<T> FullEthApiTypes for T where T: RpcNodeCore + EthApiTypes {}
