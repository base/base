//! Admin JSON-RPC trait and server implementation.

use base_batcher_core::{
    AdminError, AdminHandle, BatcherStatus, ThrottleConfig, ThrottleInfo, ThrottleStrategy,
};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    types::{ErrorCode, ErrorObjectOwned},
};
use tracing::warn;

#[rpc(server, namespace = "admin")]
pub trait BatcherAdminApi {
    /// Start block ingestion again after a previous stop. Does nothing if already running.
    #[method(name = "startBatcher")]
    async fn start_batcher(&self) -> RpcResult<()>;

    /// Stop block ingestion; the driver task keeps running. Does nothing if already stopped.
    #[method(name = "stopBatcher")]
    async fn stop_batcher(&self) -> RpcResult<()>;

    /// Flush the current encoding channel, making its frames eligible for submission.
    ///
    /// Fails if the batcher is stopped.
    #[method(name = "flushBatcher")]
    async fn flush_batcher(&self) -> RpcResult<()>;

    /// Read the current throttle controller state.
    #[method(name = "getThrottleController")]
    async fn get_throttle_controller(&self) -> RpcResult<ThrottleInfo>;

    /// Replace the throttle strategy and configuration.
    ///
    /// `config` sets the full throttle configuration, and all fields are required. Fails if
    /// `config` does not pass [`ThrottleConfig::validate`].
    #[method(name = "setThrottleController")]
    async fn set_throttle_controller(
        &self,
        strategy: ThrottleStrategy,
        config: ThrottleConfig,
    ) -> RpcResult<()>;

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
            AdminError::Stopped => -32002,
            AdminError::InvalidThrottleConfig(_) => ErrorCode::InvalidParams.code(),
        };
        ErrorObjectOwned::owned(code, e.to_string(), None::<()>)
    }
}

#[async_trait]
impl BatcherAdminApiServer for BatcherAdminApiServerImpl {
    async fn start_batcher(&self) -> RpcResult<()> {
        self.handle.start().await.map_err(Self::admin_error)
    }

    async fn stop_batcher(&self) -> RpcResult<()> {
        self.handle.stop().await.map_err(Self::admin_error)
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

    async fn get_batcher_status(&self) -> RpcResult<BatcherStatus> {
        self.handle.get_status().await.map_err(Self::admin_error)
    }

    async fn set_log_level(&self, level: String) -> RpcResult<()> {
        warn!(level = %level, "admin_setLogLevel called but not yet supported");
        self.handle.set_log_level(level).map_err(Self::admin_error)
    }
}
