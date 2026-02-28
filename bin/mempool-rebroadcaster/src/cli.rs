use clap::Parser;
use mempool_rebroadcaster::RebroadcasterConfig;

use crate::LogArgs;

/// CLI entry point for the mempool rebroadcaster service.
#[derive(Parser, Debug)]
#[command(author, version, about = "A mempool rebroadcaster service")]
pub(crate) struct Args {
    #[arg(long, env, required = true)]
    geth_mempool_endpoint: String,

    #[arg(long, env, required = true)]
    reth_mempool_endpoint: String,

    #[command(flatten)]
    pub log: LogArgs,
}

impl From<Args> for RebroadcasterConfig {
    fn from(args: Args) -> Self {
        Self {
            geth_mempool_endpoint: args.geth_mempool_endpoint,
            reth_mempool_endpoint: args.reth_mempool_endpoint,
        }
    }
}
