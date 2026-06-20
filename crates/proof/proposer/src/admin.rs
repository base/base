//! Optional admin JSON-RPC handler.
//!
//! Provides `admin_startProposer`, `admin_stopProposer`, and `admin_proposerRunning` JSON-RPC
//! methods for controlling the proposer driver at runtime.

use std::{net::SocketAddr, sync::Arc};

use base_proof_contracts::{AnchorStateRegistryClient, DisputeGameFactoryClient};
use base_proof_rpc::{L1Provider, L2Provider, RollupProvider};
use eyre::Context;
use jsonrpsee::{
    core::RpcResult,
    server::{RpcModule, Server, ServerHandle},
    types::ErrorObjectOwned,
};
use tracing::info;

use crate::driver::PipelineHandle;

/// Admin JSON-RPC server backed by a [`PipelineHandle`].
#[derive(Debug)]
pub struct ProposerAdminApiServerImpl;

impl ProposerAdminApiServerImpl {
    /// Bind and start the admin server on the given socket address.
    pub async fn spawn<L1, L2, R, ASR, F>(
        addr: SocketAddr,
        driver: Arc<PipelineHandle<L1, L2, R, ASR, F>>,
    ) -> eyre::Result<ServerHandle>
    where
        L1: L1Provider + 'static,
        L2: L2Provider + 'static,
        R: RollupProvider + 'static,
        ASR: AnchorStateRegistryClient + 'static,
        F: DisputeGameFactoryClient + 'static,
    {
        let server =
            Server::builder().build(addr).await.wrap_err("failed to bind admin RPC server")?;
        let local_addr =
            server.local_addr().wrap_err("failed to get admin server local address")?;
        let module = Self::module(driver)?;
        info!(addr = %local_addr, "admin RPC server listening");
        Ok(server.start(module))
    }

    /// Build the admin RPC module.
    pub fn module<L1, L2, R, ASR, F>(
        driver: Arc<PipelineHandle<L1, L2, R, ASR, F>>,
    ) -> eyre::Result<RpcModule<()>>
    where
        L1: L1Provider + 'static,
        L2: L2Provider + 'static,
        R: RollupProvider + 'static,
        ASR: AnchorStateRegistryClient + 'static,
        F: DisputeGameFactoryClient + 'static,
    {
        let mut module = RpcModule::new(());

        let start_driver = Arc::clone(&driver);
        module
            .register_async_method("admin_startProposer", move |_, _, _| {
                let driver = Arc::clone(&start_driver);
                async move { driver.start_proposer().await.map_err(Self::rpc_error) }
            })
            .wrap_err("failed to register admin_startProposer")?;

        let stop_driver = Arc::clone(&driver);
        module
            .register_async_method("admin_stopProposer", move |_, _, _| {
                let driver = Arc::clone(&stop_driver);
                async move { driver.stop_proposer().await.map_err(Self::rpc_error) }
            })
            .wrap_err("failed to register admin_stopProposer")?;

        module
            .register_method("admin_proposerRunning", move |_, _, _| {
                RpcResult::Ok(driver.is_running())
            })
            .wrap_err("failed to register admin_proposerRunning")?;

        Ok(module)
    }

    /// Convert a driver control error into a JSON-RPC error object.
    pub fn rpc_error(msg: &'static str) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(-32000, msg, None::<()>)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::test_utils::test_pipeline_handle;

    #[tokio::test]
    async fn module_registers_admin_methods() {
        let cancel = CancellationToken::new();
        let driver = Arc::new(test_pipeline_handle(cancel));
        let module = ProposerAdminApiServerImpl::module(Arc::clone(&driver)).unwrap();
        let methods = module.method_names().collect::<Vec<_>>();

        assert!(methods.contains(&"admin_startProposer"));
        assert!(methods.contains(&"admin_stopProposer"));
        assert!(methods.contains(&"admin_proposerRunning"));

        let running: bool = module.call("admin_proposerRunning", Vec::<()>::new()).await.unwrap();
        assert!(!running);

        module.call::<_, ()>("admin_startProposer", Vec::<()>::new()).await.unwrap();
        let running: bool = module.call("admin_proposerRunning", Vec::<()>::new()).await.unwrap();
        assert!(running);

        module.call::<_, ()>("admin_stopProposer", Vec::<()>::new()).await.unwrap();
        let running: bool = module.call("admin_proposerRunning", Vec::<()>::new()).await.unwrap();
        assert!(!running);
    }
}
