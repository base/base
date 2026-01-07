//! Mempool command - streams pending transactions via WebSocket.

use alloy_chains::Chain;
// Re-export traits for accessing tx fields
use alloy_provider::network::TransactionResponse;
use alloy_provider::{Provider, ProviderBuilder, WsConnect};
use alloy_rpc_types_eth::{Transaction, TransactionTrait};
use anyhow::Result;
use clap::Args;
use futures_util::StreamExt;

use crate::flags::GlobalArgs;

/// Returns the default public WebSocket RPC URL for the given chain.
///
/// These are free, public endpoints from PublicNode that support eth_subscribe.
/// See: <https://chainlist.org/chain/8453>
fn default_ws_url(chain: &Chain) -> Option<&'static str> {
    match chain.id() {
        8453 => Some("wss://base-rpc.publicnode.com"), // Base Mainnet
        84532 => Some("wss://base-sepolia-rpc.publicnode.com"), // Base Sepolia
        1 => Some("wss://ethereum-rpc.publicnode.com"), // Ethereum Mainnet
        11155111 => Some("wss://ethereum-sepolia-rpc.publicnode.com"), // Ethereum Sepolia
        _ => None,
    }
}

/// Prints a transaction to stdout in human-readable format.
fn print_transaction(tx: &Transaction) {
    println!("tx: {}", tx.tx_hash());
    println!("  from:  {}", tx.from());
    if let Some(to) = tx.to() {
        println!("  to:    {to}");
    }
    println!("  value: {}", tx.value());
    println!("  gas:   {}", tx.gas_limit());
    println!();
}

/// The mempool command - streams pending transactions via WebSocket.
///
/// Uses free public WebSocket endpoints by default (from PublicNode).
/// You can override with `--rpc-url` for a custom endpoint.
#[derive(Debug, Clone, Args)]
pub struct MempoolCommand {
    /// WebSocket RPC URL. Defaults to a free public endpoint for supported networks.
    ///
    /// Examples:
    ///   - PublicNode: wss://base-rpc.publicnode.com (default for Base)
    ///   - Alchemy: wss://base-mainnet.g.alchemy.com/v2/YOUR_API_KEY
    ///   - QuickNode: wss://YOUR_ENDPOINT.base-mainnet.quiknode.pro/YOUR_API_KEY
    #[arg(long = "rpc-url", env = "BASE_RPC_URL")]
    pub rpc_url: Option<String>,

    /// Show full transaction details instead of just hashes.
    #[arg(long = "full", short = 'f', default_value = "false")]
    pub full: bool,
}

impl MempoolCommand {
    /// Runs the mempool command.
    pub async fn run(&self, global: &GlobalArgs) -> Result<()> {
        // Determine WebSocket URL
        let ws_url = match &self.rpc_url {
            Some(url) => url.clone(),
            None => default_ws_url(&global.network)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "No default WebSocket URL for network {}. Provide --rpc-url.",
                        global.network
                    )
                })?
                .to_string(),
        };

        println!("Connecting to {}...", ws_url);

        // Connect to WebSocket provider
        let ws = WsConnect::new(&ws_url);
        let provider = ProviderBuilder::new().connect_ws(ws).await?;

        println!("Connected! Subscribing to pending transactions on {}...", global.network);

        // Subscribe to pending transactions
        let sub = provider.subscribe_pending_transactions().await?;
        let mut stream = sub.into_stream();

        println!("Subscribed! Streaming transactions (Ctrl-C to stop)...");
        println!();

        // Stream transactions
        while let Some(tx_hash) = stream.next().await {
            if self.full {
                // Fetch full transaction details
                match provider.get_transaction_by_hash(tx_hash).await {
                    Ok(Some(tx)) => {
                        print_transaction(&tx);
                    }
                    Ok(None) => println!("tx: {tx_hash} (not found)"),
                    Err(e) => {
                        eprintln!("Failed to fetch tx {}: {}", tx_hash, e);
                    }
                }
            } else {
                println!("tx: {tx_hash}");
            }
        }

        Ok(())
    }
}
