//! Base node extension that starts transaction ingress.

use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use eyre::WrapErr;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tracing::{error, info};

use crate::{TransactionIngressConfig, TransactionIngressService};

/// Node extension that serves private streamed transaction submission.
#[derive(Debug)]
pub struct TransactionIngressExtension {
    config: TransactionIngressConfig,
}

impl TransactionIngressExtension {
    /// Creates a transaction ingress extension.
    pub const fn new(config: TransactionIngressConfig) -> Self {
        Self { config }
    }
}

impl BaseNodeExtension for TransactionIngressExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let Some(listen_addr) = self.config.listen_addr else {
            return hooks;
        };

        hooks.add_rpc_module(move |ctx| {
            let listener = std::net::TcpListener::bind(listen_addr)
                .wrap_err_with(|| format!("failed to bind transaction ingress to {listen_addr}"))?;
            listener.set_nonblocking(true)?;
            let listener = tokio::net::TcpListener::from_std(listener)?;
            let incoming = TcpListenerStream::new(listener);
            let service = TransactionIngressService::new(ctx.registry.eth_api().clone());
            let service = crate::protocol::transaction_ingress_service_server::TransactionIngressServiceServer::new(service);
            let executor = ctx.node().task_executor.clone();

            info!(address = %listen_addr, "starting transaction ingress server");
            executor.spawn_with_graceful_shutdown_signal(move |signal| {
                Box::pin(async move {
                    let shutdown = async move {
                        let _guard = signal.await;
                    };
                    if let Err(error) = Server::builder()
                        .add_service(service)
                        .serve_with_incoming_shutdown(incoming, shutdown)
                        .await
                    {
                        error!(error = %error, "transaction ingress server stopped unexpectedly");
                    }
                })
            });

            Ok(())
        })
    }
}

impl FromExtensionConfig for TransactionIngressExtension {
    type Config = TransactionIngressConfig;

    fn from_config(config: Self::Config) -> Self {
        Self::new(config)
    }
}
