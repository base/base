//! Error handling and conversion for the Base `eth` API.

use std::error::Error;

use reth_rpc_convert::RpcConvert;

use crate::{AsEthApiError, FromEthApiError, RpcNodeCore};

/// Errors and stateful conversion used by the Base `eth_` and adjacent endpoints.
///
/// Converters provide context such as deposit metadata and L1 receipt fees.
pub trait EthApiTypes: Send + Sync + Clone {
    /// Extension of [`FromEthApiError`], with network specific errors.
    type Error: Into<jsonrpsee_types::error::ErrorObject<'static>>
        + FromEthApiError
        + AsEthApiError
        + From<<Self::RpcConvert as RpcConvert>::Error>
        + Error
        + Send
        + Sync;

    /// Conversion methods for transaction RPC type.
    type RpcConvert: RpcConvert;

    /// Returns reference to transaction response builder.
    fn converter(&self) -> &Self::RpcConvert;
}

/// Adapter for network specific error type.
pub type RpcError<T> = <T as EthApiTypes>::Error;

/// Helper trait holds necessary trait bounds on [`EthApiTypes`] to implement `eth` API.
pub trait FullEthApiTypes
where
    Self: RpcNodeCore + EthApiTypes<RpcConvert: RpcConvert>,
{
}

impl<T> FullEthApiTypes for T where T: RpcNodeCore + EthApiTypes<RpcConvert: RpcConvert> {}
