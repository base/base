//! Admin JSON-RPC trait and server implementation.

use base_batcher_core::{
    AdminError, AdminHandle, BatcherStatus, ThrottleConfig, ThrottleInfo, ThrottleStrategy,
};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    types::ErrorObjectOwned,
};
use tracing::warn;

#[rpc(server, namespace = "admin")]
pub trait BatcherAdminApi {
    /// Resume block ingestion after a previous stop.
    #[method(name = "startBatcher")]
    async fn start_batcher(&self) -> RpcResult<()>;

    /// Pause block ingestion without stopping the driver task.
    #[method(name = "stopBatcher")]
    async fn stop_batcher(&self) -> RpcResult<()>;

    /// Force-close the current encoding channel, submitting any buffered frames.
    #[method(name = "flushBatcher")]
    async fn flush_batcher(&self) -> RpcResult<()>;

    /// Read the current throttle controller state.
    #[method(name = "getThrottleController")]
    async fn get_throttle_controller(&self) -> RpcResult<ThrottleInfo>;

    /// Replace the throttle strategy and configuration.
    ///
    /// `config` sets the full throttle configuration; all fields are required.
    #[method(name = "setThrottleController")]
    async fn set_throttle_controller(
        &self,
        strategy: ThrottleStrategy,
        config: ThrottleConfig,
    ) -> RpcResult<()>;

    /// Clear the throttle dedup cache so limits are re-applied unconditionally.
    #[method(name = "resetThrottleController")]
    async fn reset_throttle_controller(&self) -> RpcResult<()>;

    /// Read the current driver runtime state.
    #[method(name = "getBatcherStatus")]
    async fn get_batcher_status(&self) -> RpcResult<BatcherStatus>;

    /// Set the log level (not yet supported; returns an error).
    #[method(name = "setLogLevel")]
    async fn set_log_level(&self, level: String) -> RpcResult<()>;
}

/// Concrete implementation of [`BatcherAdminApiServer`] backed by an [`AdminHandle`].
#[derive(Debug)]
pub struct BatcherAdminApiServerImpl {
    handle: AdminHandle,
}

impl BatcherAdminApiServerImpl {
    /// Create a new server implementation backed by `handle`.
    pub const fn new(handle: AdminHandle) -> Self {
        Self { handle }
    }

    /// Convert an [`AdminError`] into a JSON-RPC error object.
    fn admin_error(e: AdminError) -> ErrorObjectOwned {
        let code = match e {
            AdminError::NotSupported(_) => -32601,
            AdminError::ChannelClosed => -32001,
        };
        ErrorObjectOwned::owned(code, e.to_string(), None::<()>)
    }
}

#[async_trait]
impl BatcherAdminApiServer for BatcherAdminApiServerImpl {
    async fn start_batcher(&self) -> RpcResult<()> {
        self.handle.resume().await.map_err(Self::admin_error)
    }

    async fn stop_batcher(&self) -> RpcResult<()> {
        self.handle.pause().await.map_err(Self::admin_error)
    }

    async fn flush_batcher(&self) -> RpcResult<()> {
        self.handle.flush().await.map_err(Self::admin_error)
    }

    async fn get_throttle_controller(&self) -> RpcResult<ThrottleInfo> {
        self.handle.get_throttle_info().await.map_err(Self::admin_error)
    }

    async fn set_throttle_controller(
        &self,
        strategy: ThrottleStrategy,
        config: ThrottleConfig,
    ) -> RpcResult<()> {
        self.handle.set_throttle(strategy, config).await.map_err(Self::admin_error)
    }

    async fn reset_throttle_controller(&self) -> RpcResult<()> {
        self.handle.reset_throttle().await.map_err(Self::admin_error)
    }

    async fn get_batcher_status(&self) -> RpcResult<BatcherStatus> {
        self.handle.get_status().await.map_err(Self::admin_error)
    }

    async fn set_log_level(&self, level: String) -> RpcResult<()> {
        warn!(level = %level, "admin_setLogLevel called but not yet supported");
        self.handle.set_log_level(level).map_err(Self::admin_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_error_not_supported_uses_method_not_found_code() {
        let err = BatcherAdminApiServerImpl::admin_error(AdminError::NotSupported("test"));
        assert_eq!(err.code(), -32601);
        assert!(err.message().contains("not yet supported"));
    }

    #[test]
    fn admin_error_channel_closed_uses_server_error_code() {
        let err = BatcherAdminApiServerImpl::admin_error(AdminError::ChannelClosed);
        assert_eq!(err.code(), -32001);
    }
}
