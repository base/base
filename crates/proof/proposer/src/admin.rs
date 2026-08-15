//! Optional admin JSON-RPC handler.
//!
//! Provides `admin_startProposer`, `admin_stopProposer`, and `admin_proposerRunning` JSON-RPC
//! methods for controlling the proposer driver at runtime.

use std::{net::SocketAddr, sync::Arc};

use eyre::Context;
use jsonrpsee::{
    core::RpcResult,
    server::{RpcModule, Server, ServerHandle},
    types::ErrorObjectOwned,
};
use tracing::info;

use crate::driver::ProposerDriverControl;

/// Admin JSON-RPC server backed by a [`ProposerDriverControl`] handle.
#[derive(Debug)]
pub struct ProposerAdminApiServerImpl;

impl ProposerAdminApiServerImpl {
    /// Bind and start the admin server on the given socket address.
    pub async fn spawn(
        addr: SocketAddr,
        driver: Arc<dyn ProposerDriverControl>,
    ) -> eyre::Result<ServerHandle> {
        let server =
            Server::builder().build(addr).await.wrap_err("failed to bind admin RPC server")?;
        let local_addr =
            server.local_addr().wrap_err("failed to get admin server local address")?;
        let module = Self::module(driver)?;
        info!(addr = %local_addr, "admin RPC server listening");
        Ok(server.start(module))
    }

    /// Build the admin RPC module.
    pub fn module(driver: Arc<dyn ProposerDriverControl>) -> eyre::Result<RpcModule<()>> {
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
    pub fn rpc_error(msg: String) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(-32000, msg, None::<()>)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use async_trait::async_trait;

    use super::*;

    #[derive(Debug, Default)]
    struct MockDriver {
        running: AtomicBool,
    }

    #[async_trait]
    impl ProposerDriverControl for MockDriver {
        async fn start_proposer(&self) -> Result<(), String> {
            self.running.store(true, Ordering::Release);
            Ok(())
        }

        async fn stop_proposer(&self) -> Result<(), String> {
            self.running.store(false, Ordering::Release);
            Ok(())
        }

        fn is_running(&self) -> bool {
            self.running.load(Ordering::Acquire)
        }
    }

    #[tokio::test]
    async fn module_registers_admin_methods() {
        let driver: Arc<dyn ProposerDriverControl> = Arc::new(MockDriver::default());
        let module = ProposerAdminApiServerImpl::module(driver).unwrap();
        let params = Vec::<()>::new();

        let running: bool = module.call("admin_proposerRunning", params.clone()).await.unwrap();
        assert!(!running);

        let _: () = module.call("admin_startProposer", params.clone()).await.unwrap();
        let running: bool = module.call("admin_proposerRunning", params.clone()).await.unwrap();
        assert!(running);

        let _: () = module.call("admin_stopProposer", params.clone()).await.unwrap();
        let running: bool = module.call("admin_proposerRunning", params).await.unwrap();
        assert!(!running);
    }
}
