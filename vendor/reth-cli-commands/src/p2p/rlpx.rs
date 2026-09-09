//! RLPx subcommand of P2P Debugging tool.

use base_execution_network_types::{AnyNode, pk2id};
use base_execution_network_wire::ECIESStream;
use base_execution_network_wire::HelloMessage;
use base_execution_network_wire::UnauthedP2PStream;
use clap::{Parser, Subcommand};
use reth_network::config::rng_secret_key;
use secp256k1::SECP256K1;
use tokio::net::TcpStream;

/// RLPx commands
#[derive(Parser, Debug)]
pub struct Command {
    #[command(subcommand)]
    subcommand: Subcommands,
}

impl Command {
    // Execute `p2p rlpx` command.
    pub async fn execute(self) -> eyre::Result<()> {
        match self.subcommand {
            Subcommands::Ping { node } => {
                let key = rng_secret_key();
                let node_record = node
                    .node_record()
                    .ok_or_else(|| eyre::eyre!("failed to parse node {}", node))?;
                let outgoing =
                    TcpStream::connect((node_record.address, node_record.tcp_port)).await?;
                let ecies_stream = ECIESStream::connect(outgoing, key, node_record.id).await?;

                let peer_id = pk2id(&key.public_key(SECP256K1));
                let hello = HelloMessage::builder(peer_id).build();

                let (_, their_hello) =
                    UnauthedP2PStream::new(ecies_stream).handshake(hello).await?;

                println!("{their_hello:#?}");
            }
        }
        Ok(())
    }
}

#[derive(Subcommand, Debug)]
enum Subcommands {
    /// ping node
    Ping {
        /// The node to ping.
        node: AnyNode,
    },
}
