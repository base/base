//! Error handling and conversion for the Base `eth` API.

/// Errors and stateful conversion used by the Base `eth_` and adjacent endpoints.
///
/// Converters provide context such as deposit metadata and L1 receipt fees.
pub trait EthApiTypes: RpcNodeCore + Send + Sync + Clone {
    /// Returns reference to transaction response builder.
    fn converter(&self) -> &crate::BaseRpcConverter<Self::Provider>;
}

use crate::RpcNodeCore;
