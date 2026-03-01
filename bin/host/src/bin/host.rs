//! Main entrypoint for the host binary.

#![warn(missing_debug_implementations, missing_docs, unreachable_pub, rustdoc::all)]
#![deny(unused_must_use, rust_2018_idioms)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde::Serialize;
use tracing::info;
use tracing_subscriber::{
    EnvFilter, fmt, prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt,
};

const ABOUT: &str = "
base-proof-host is a CLI application that runs the Base pre-image server and client program. The host
can run in two modes: server mode and native mode. In server mode, the host runs the pre-image
server and waits for the client program in the parent process to request pre-images. In native
mode, the host runs the client program in a separate thread with the pre-image server in the
primary thread.
";

/// The host binary CLI application arguments.
#[derive(Parser, Serialize, Clone, Debug)]
#[command(about = ABOUT, version)]
pub struct HostCli {
    /// Verbosity level (0-2)
    #[arg(short, action = clap::ArgAction::Count)]
    pub v: u8,
    /// Host mode
    #[command(subcommand)]
    pub mode: HostMode,
}

/// Operation modes for the host binary.
#[derive(Subcommand, Serialize, Clone, Debug)]
pub enum HostMode {
    /// Run the host in single-chain mode.
    #[cfg(feature = "single")]
    Single(Box<base_host::single::SingleChainHost>),
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cfg = HostCli::parse();

    // Initialize tracing based on verbosity
    let level = match cfg.v {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    tracing_subscriber::registry().with(fmt::layer()).with(env_filter).init();

    match cfg.mode {
        #[cfg(feature = "single")]
        HostMode::Single(cfg) => {
            (*cfg).start().await?;
        }
    }

    info!(target: "host", "Exiting host program");
    Ok(())
}
