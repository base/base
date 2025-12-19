//! Contains the CLI arguments

use std::sync::Arc;

use alloy_primitives::Address;
use base_reth_runner::{
    BaseNodeConfig, EncryptedRelayConfig, FlashblocksCell, FlashblocksConfig, RelayFetcherConfig,
    TracingConfig,
};
use once_cell::sync::OnceCell;
use reth_optimism_node::args::RollupArgs;

/// CLI Arguments
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
pub struct Args {
    /// Rollup arguments
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    /// The websocket url used for flashblocks.
    #[arg(long = "websocket-url", value_name = "WEBSOCKET_URL")]
    pub websocket_url: Option<String>,

    /// The mac pending blocks depth.
    #[arg(
        long = "max-pending-blocks-depth",
        value_name = "MAX_PENDING_BLOCKS_DEPTH",
        default_value = "3"
    )]
    pub max_pending_blocks_depth: u64,

    /// Enable transaction tracing ExEx for mempool-to-block timing analysis
    #[arg(long = "enable-transaction-tracing", value_name = "ENABLE_TRANSACTION_TRACING")]
    pub enable_transaction_tracing: bool,

    /// Enable `info` logs for transaction tracing
    #[arg(
        long = "enable-transaction-tracing-logs",
        value_name = "ENABLE_TRANSACTION_TRACING_LOGS"
    )]
    pub enable_transaction_tracing_logs: bool,

    /// Enable metering RPC for transaction bundle simulation
    #[arg(long = "enable-metering", value_name = "ENABLE_METERING")]
    pub enable_metering: bool,

    /// Enable encrypted transaction relay RPC endpoints
    #[arg(long = "enable-relay", value_name = "ENABLE_RELAY")]
    pub enable_relay: bool,

    /// Encryption public key for relay (32 bytes, hex-encoded)
    #[arg(long = "relay-encryption-pubkey", value_name = "HEX_PUBKEY")]
    pub relay_encryption_pubkey: Option<String>,

    /// Attestation public key for relay (32 bytes, hex-encoded)
    #[arg(long = "relay-attestation-pubkey", value_name = "HEX_PUBKEY")]
    pub relay_attestation_pubkey: Option<String>,

    /// Proof-of-work difficulty for relay (leading zero bits, default: 18)
    #[arg(long = "relay-pow-difficulty", value_name = "DIFFICULTY", default_value = "18")]
    pub relay_pow_difficulty: u8,

    /// L1 RPC URL for fetching relay config from the EncryptedRelayConfig contract
    #[arg(
        long = "relay-config-rpc-url",
        value_name = "URL",
        env = "RELAY_CONFIG_RPC_URL"
    )]
    pub relay_config_rpc_url: Option<String>,

    /// Address of the EncryptedRelayConfig contract on L1
    #[arg(
        long = "relay-config-contract",
        value_name = "ADDRESS",
        env = "RELAY_CONFIG_CONTRACT"
    )]
    pub relay_config_contract: Option<String>,

    /// Poll interval in seconds for fetching relay config updates (default: 60)
    #[arg(
        long = "relay-config-poll-interval",
        value_name = "SECONDS",
        default_value = "60",
        env = "RELAY_CONFIG_POLL_INTERVAL"
    )]
    pub relay_config_poll_interval: u64,

    /// Run as relay sequencer (decrypts and processes transactions locally)
    #[arg(
        long = "relay-sequencer",
        value_name = "RELAY_SEQUENCER",
        env = "RELAY_SEQUENCER"
    )]
    pub relay_sequencer: bool,

    /// Path to Ed25519 keypair file for sequencer (32-byte secret key)
    #[arg(
        long = "relay-keypair",
        value_name = "PATH",
        env = "RELAY_KEYPAIR"
    )]
    pub relay_keypair: Option<String>,

    /// Hex-encoded Ed25519 keypair for sequencer (64 hex chars, optional 0x prefix)
    /// Alternative to --relay-keypair for devnet/testing where file mounting is inconvenient
    #[arg(
        long = "relay-keypair-hex",
        value_name = "HEX",
        env = "RELAY_KEYPAIR_HEX"
    )]
    pub relay_keypair_hex: Option<String>,

    /// Generate a new keypair on startup (for devnet only)
    #[arg(
        long = "relay-keypair-generate",
        value_name = "RELAY_KEYPAIR_GENERATE",
        env = "RELAY_KEYPAIR_GENERATE"
    )]
    pub relay_keypair_generate: bool,

    /// URL of the sequencer's RPC for HTTP forwarding (relay mode only)
    /// When set, encrypted transactions are forwarded to this URL instead of being processed locally
    #[arg(
        long = "relay-sequencer-url",
        value_name = "URL",
        env = "RELAY_SEQUENCER_URL"
    )]
    pub relay_sequencer_url: Option<String>,
}

impl Args {
    /// Returns if flashblocks is enabled.
    /// If the websocket url is specified through the CLI.
    pub const fn flashblocks_enabled(&self) -> bool {
        self.websocket_url.is_some()
    }

    /// Returns if encrypted relay is enabled.
    pub const fn relay_enabled(&self) -> bool {
        self.enable_relay
    }

    /// Parses the relay encryption public key from hex.
    fn parse_relay_pubkey(hex: &Option<String>) -> Option<[u8; 32]> {
        hex.as_ref().and_then(|s| {
            let s = s.strip_prefix("0x").unwrap_or(s);
            let bytes = hex::decode(s).ok()?;
            if bytes.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                Some(arr)
            } else {
                None
            }
        })
    }
}

impl From<Args> for BaseNodeConfig {
    fn from(args: Args) -> Self {
        let flashblocks_cell: FlashblocksCell = Arc::new(OnceCell::new());
        let flashblocks = args.websocket_url.clone().map(|websocket_url| FlashblocksConfig {
            websocket_url,
            max_pending_blocks_depth: args.max_pending_blocks_depth,
        });

        let relay = if args.enable_relay {
            // Build fetcher config if both RPC URL and contract address are provided
            let fetcher = match (&args.relay_config_rpc_url, &args.relay_config_contract) {
                (Some(rpc_url), Some(contract_str)) => {
                    // Parse the contract address (with or without 0x prefix)
                    let contract_str = contract_str.strip_prefix("0x").unwrap_or(contract_str);
                    match contract_str.parse::<Address>() {
                        Ok(contract_address) => Some(RelayFetcherConfig {
                            rpc_url: rpc_url.clone(),
                            contract_address,
                            poll_interval_secs: args.relay_config_poll_interval,
                        }),
                        Err(_) => {
                            tracing::warn!(
                                address = %args.relay_config_contract.as_ref().unwrap(),
                                "Invalid relay config contract address, polling disabled"
                            );
                            None
                        }
                    }
                }
                _ => None,
            };

            Some(EncryptedRelayConfig {
                encryption_pubkey: Args::parse_relay_pubkey(&args.relay_encryption_pubkey)
                    .unwrap_or([0u8; 32]),
                attestation_pubkey: Args::parse_relay_pubkey(&args.relay_attestation_pubkey)
                    .unwrap_or([0u8; 32]),
                pow_difficulty: args.relay_pow_difficulty,
                fetcher,
                is_sequencer: args.relay_sequencer,
                keypair_path: args.relay_keypair.clone(),
                keypair_hex: args.relay_keypair_hex.clone(),
                keypair_generate: args.relay_keypair_generate,
                sequencer_url: args.relay_sequencer_url.clone(),
            })
        } else {
            None
        };

        Self {
            rollup_args: args.rollup_args,
            flashblocks,
            tracing: TracingConfig {
                enabled: args.enable_transaction_tracing,
                logs_enabled: args.enable_transaction_tracing_logs,
            },
            metering_enabled: args.enable_metering,
            flashblocks_cell,
            relay,
        }
    }
}
