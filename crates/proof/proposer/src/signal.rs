//! Signal handling for graceful shutdown.

use tokio_util::sync::CancellationToken;
use tracing::info;

/// Handles OS signals for graceful shutdown.
#[derive(Debug)]
pub struct SignalHandler;

impl SignalHandler {
    /// Installs SIGTERM + SIGINT handlers that cancel the given token.
    pub fn install(cancel: CancellationToken) {
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut sigterm =
                    signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        result.expect("failed to listen for SIGINT");
                        info!("Received SIGINT");
                    }
                    _ = sigterm.recv() => {
                        info!("Received SIGTERM");
                    }
                }
            }

            #[cfg(not(unix))]
            {
                tokio::signal::ctrl_c().await.expect("failed to listen for SIGINT");
                info!("Received SIGINT");
            }

            cancel.cancel();
        });
    }
}
