//! P2P subcommands, overriding the upstream bootnode with a discv5 NAT fix.

use std::sync::Arc;

use alloy_eips::BlockHashOrNumber;
use backon::Retryable;
use base_execution_chainspec::BaseChainSpec;
use clap::{Parser, Subcommand};
use reth_cli_commands::{
    common::CliNodeTypes,
    p2p::{DownloadArgs, enode, rlpx},
};
use reth_cli_util::hash_or_num_value_parser;
use reth_network::{BlockDownloaderProvider, NetworkHandle};
use reth_network_p2p::bodies::client::BodiesClient;
use reth_node_core::utils::get_single_header;

pub mod bootnode;

/// P2P debugging utilities.
#[derive(Debug, Parser)]
pub struct Command {
    #[command(subcommand)]
    command: Subcommands,
}

impl Command {
    /// Execute the p2p command.
    pub async fn execute<N>(self) -> eyre::Result<()>
    where
        N: CliNodeTypes<ChainSpec = BaseChainSpec>,
        NetworkHandle<N::NetworkPrimitives>: BlockDownloaderProvider,
    {
        match self.command {
            Subcommands::Header { args, id } => {
                let handle = args.launch_network::<N>().await?;
                let fetch_client = handle.fetch_client().await?;
                let backoff = args.backoff();

                let header = (move || get_single_header(fetch_client.clone(), id))
                    .retry(backoff)
                    .notify(|err, _| {
                        tracing::warn!(target: "reth::cli", error = %err, "Error requesting header. Retrying...")
                    })
                    .await?;
                tracing::info!(target: "reth::cli", ?header, "Successfully downloaded header");
            }
            Subcommands::Body { args, id } => {
                let handle = args.launch_network::<N>().await?;
                let fetch_client = handle.fetch_client().await?;
                let backoff = args.backoff();

                let hash = match id {
                    BlockHashOrNumber::Hash(hash) => hash,
                    BlockHashOrNumber::Number(number) => {
                        tracing::info!(target: "reth::cli", "Block number provided. Downloading header first...");
                        let client = fetch_client.clone();
                        let header = (move || {
                            get_single_header(
                                client.clone(),
                                BlockHashOrNumber::Number(number),
                            )
                        })
                        .retry(backoff)
                        .notify(|err, _| {
                            tracing::warn!(target: "reth::cli", error = %err, "Error requesting header. Retrying...")
                        })
                        .await?;
                        header.hash()
                    }
                };

                let (_, result) = (move || {
                    let client = fetch_client.clone();
                    client.get_block_bodies(vec![hash])
                })
                .retry(backoff)
                .notify(|err, _| {
                    tracing::warn!(target: "reth::cli", error = %err, "Error requesting block. Retrying...")
                })
                .await?
                .split();

                if result.len() != 1 {
                    eyre::bail!(
                        "Invalid number of bodies received. Expected: 1. Received: {}",
                        result.len()
                    )
                }
                let body = result.into_iter().next().unwrap();
                tracing::info!(target: "reth::cli", ?body, "Successfully downloaded body");
            }
            Subcommands::Rlpx(command) => {
                command.execute().await?;
            }
            Subcommands::Bootnode(command) => {
                command.execute().await?;
            }
            Subcommands::Enode(command) => {
                command.execute()?;
            }
        }

        Ok(())
    }

    /// Returns the chain spec if one is embedded in the active subcommand.
    ///
    /// Header and Body delegate chain parsing internally to [`DownloadArgs`] whose `chain`
    /// field is private, so we return `None` for those. Only the log-directory path suffix
    /// uses this value, so the impact is cosmetic.
    pub const fn chain_spec(&self) -> Option<&Arc<BaseChainSpec>> {
        None
    }
}

#[derive(Subcommand, Debug)]
enum Subcommands {
    /// Download a block header by number or hash.
    Header {
        #[command(flatten)]
        args: DownloadArgs<crate::chainspec::BaseChainSpecParser>,
        /// Block number or hash to fetch.
        #[arg(value_parser = hash_or_num_value_parser)]
        id: BlockHashOrNumber,
    },
    /// Download a block body by number or hash.
    Body {
        #[command(flatten)]
        args: DownloadArgs<crate::chainspec::BaseChainSpecParser>,
        /// Block number or hash to fetch.
        #[arg(value_parser = hash_or_num_value_parser)]
        id: BlockHashOrNumber,
    },
    /// `RLPx` utilities.
    Rlpx(rlpx::Command),
    /// Start a discovery-only bootnode.
    Bootnode(bootnode::Command),
    /// Print the enode identifier of this node.
    Enode(enode::Command),
}
