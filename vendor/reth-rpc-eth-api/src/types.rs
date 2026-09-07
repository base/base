//! Error handling and conversion for the Base `eth` API.

use std::error::Error;

use crate::{AsEthApiError, FromEthApiError, RpcNodeCore};

/// Errors and stateful conversion used by the Base `eth_` and adjacent endpoints.
///
/// Converters provide context such as deposit metadata and L1 receipt fees.
pub trait EthApiTypes: RpcNodeCore + Send + Sync + Clone {
    /// Extension of [`FromEthApiError`], with network specific errors.
    type Error: Into<jsonrpsee_types::error::ErrorObject<'static>>
        + FromEthApiError
        + AsEthApiError
        + From<reth_rpc_eth_types::BaseEthApiError>
        + Error
        + Send
        + Sync;

    /// Returns reference to transaction response builder.
    fn converter(&self) -> &crate::BaseRpcConverter<Self::Provider>;
}

/// Adapter for network specific error type.
pub type RpcError<T> = <T as EthApiTypes>::Error;

/// Helper trait holds necessary trait bounds on [`EthApiTypes`] to implement `eth` API.
pub trait FullEthApiTypes
where
    Self: RpcNodeCore + EthApiTypes,
{
}

impl<T> FullEthApiTypes for T where T: RpcNodeCore + EthApiTypes {}
