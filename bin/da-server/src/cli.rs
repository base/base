use base_alt_da::{Config, Server};
use base_cli_utils::RuntimeManager;
use clap::Parser;

/// Alt-DA HTTP server for L3 batch data.
#[derive(Debug, Parser)]
pub(crate) struct Cli {
    /// TCP port for the DA HTTP API.
    #[arg(long, env = "BASE_DA_PORT", default_value = "2583")]
    port: u16,

    /// Backing store URL (`s3://bucket/prefix` or `file:///path`).
    #[arg(long, env = "BASE_DA_URL")]
    da_url: String,
}

impl Cli {
    /// Run the alt-DA server until SIGINT/SIGTERM.
    pub(crate) fn run(self) -> eyre::Result<()> {
        RuntimeManager::new().run_until_shutdown(|cancel| async move {
            let server = Server::new(Config { port: self.port, da_url: self.da_url }).await?;
            server.run(cancel).await.map_err(Into::into)
        })
    }
}
