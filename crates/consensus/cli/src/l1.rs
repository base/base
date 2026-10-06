//! L1 Client CLI arguments.

use std::{num::ParseIntError, time::Duration};

use alloy_primitives::Address;
use base_common_genesis::RollupConfig;
use base_consensus_providers::L1_RPC_TIMEOUT;
use tracing::warn;
use url::Url;

const DEFAULT_L1_TRUST_RPC: bool = true;

/// L1 client arguments.
#[derive(Clone, Debug, clap::Args)]
pub struct L1ClientArgs {
    /// URL of the L1 execution client RPC API.
    #[arg(long, visible_alias = "l1", env = "BASE_NODE_L1_ETH_RPC")]
    pub l1_eth_rpc: Url,
    /// Request timeout for general L1 execution JSON-RPC calls.
    #[arg(
        long = "l1.rpc-timeout-ms",
        default_value = L1_RPC_TIMEOUT.as_millis().to_string(),
        env = "BASE_NODE_L1_RPC_TIMEOUT_MS",
        value_parser = |arg: &str| -> Result<Duration, ParseIntError> {
            Ok(Duration::from_millis(arg.parse()?))
        }
    )]
    pub l1_rpc_timeout: Duration,
    /// Whether to trust the L1 RPC.
    /// If false, block hash verification is performed for all retrieved blocks.
    #[arg(
        long,
        visible_alias = "l1.trust-rpc",
        env = "BASE_NODE_L1_TRUST_RPC",
        default_value_t = DEFAULT_L1_TRUST_RPC
    )]
    pub l1_trust_rpc: bool,
    /// URL of the L1 beacon API.
    #[arg(long, visible_alias = "l1.beacon", env = "BASE_NODE_L1_BEACON")]
    pub l1_beacon: Url,
    /// Duration in seconds of an L1 slot.
    ///
    /// This is an optional argument that can be used to use a fixed slot duration for l1 blocks
    /// and bypass the initial beacon spec fetch. This is useful for testing purposes when the
    /// l1-beacon spec endpoint is not available (with anvil for example).
    #[arg(
        long,
        visible_alias = "l1.slot-duration-override",
        env = "BASE_NODE_L1_SLOT_DURATION_OVERRIDE"
    )]
    pub l1_slot_duration_override: Option<u64>,
    /// Dangerous validator-only override for the sender accepted by the L1 data-availability
    /// pipeline. This does not modify the protocol `SystemConfig` or L1 info transactions.
    #[arg(
        long = "l1.dangerously-override-da-batcher-sender",
        env = "BASE_NODE_L1_DANGEROUSLY_OVERRIDE_DA_BATCHER_SENDER"
    )]
    pub l1_da_batcher_sender_override: Option<Address>,
    /// Dangerous validator-only override for the batch inbox accepted by the L1
    /// data-availability pipeline.
    #[arg(
        long = "l1.dangerously-override-da-batch-inbox",
        env = "BASE_NODE_L1_DANGEROUSLY_OVERRIDE_DA_BATCH_INBOX"
    )]
    pub l1_da_batch_inbox_override: Option<Address>,
    /// Number of L1 blocks to keep distance from the L1 head for the verifier (derivation
    /// pipeline). Controlled via `BASE_NODE_VERIFIER_L1_CONFS`. Defaults to 0, meaning
    /// the verifier derives from the latest L1 head with no confirmation delay.
    #[arg(long = "l1.verifier-confs", default_value = "0", env = "BASE_NODE_VERIFIER_L1_CONFS")]
    pub l1_verifier_confs: u64,
    /// Interval, in milliseconds, between polls of the L1 `finalized` block tag. This governs how
    /// quickly the node observes new L1 finality checkpoints and, in turn, advances its finalized
    /// L2 head. Controlled via `BASE_NODE_FINALIZED_POLL_INTERVAL_MS`. When unset, a
    /// chain-specific default is used (one L1 epoch, ~384s, on Ethereum mainnet/Sepolia).
    #[arg(
        long = "l1.finalized-poll-interval-ms",
        env = "BASE_NODE_FINALIZED_POLL_INTERVAL_MS",
        value_parser = |arg: &str| -> Result<Duration, ParseIntError> {
            Ok(Duration::from_millis(arg.parse()?))
        }
    )]
    pub l1_finalized_poll_interval: Option<Duration>,
}

impl Default for L1ClientArgs {
    fn default() -> Self {
        Self {
            l1_eth_rpc: Url::parse("http://localhost:8545").unwrap(),
            l1_rpc_timeout: L1_RPC_TIMEOUT,
            l1_trust_rpc: DEFAULT_L1_TRUST_RPC,
            l1_beacon: Url::parse("http://localhost:5052").unwrap(),
            l1_slot_duration_override: None,
            l1_da_batcher_sender_override: None,
            l1_da_batch_inbox_override: None,
            l1_verifier_confs: 0,
            l1_finalized_poll_interval: None,
        }
    }
}

impl L1ClientArgs {
    /// Applies the configured L1 data-availability batch inbox override.
    pub fn apply_da_batch_inbox_override(&self, config: &mut RollupConfig) {
        let Some(inbox) = self.l1_da_batch_inbox_override else {
            return;
        };
        if config.batch_inbox_address != inbox {
            warn!(
                configured_inbox = %config.batch_inbox_address,
                override_inbox = %inbox,
                "overriding the L1 data-availability batch inbox filter"
            );
            config.batch_inbox_address = inbox;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use clap::Parser;

    use super::{L1_RPC_TIMEOUT, L1ClientArgs};

    #[derive(Parser)]
    struct Command {
        #[command(flatten)]
        args: L1ClientArgs,
    }

    #[test]
    fn defaults_l1_rpc_timeout_to_fifteen_seconds() {
        assert_eq!(L1ClientArgs::default().l1_rpc_timeout, L1_RPC_TIMEOUT);
    }

    #[test]
    fn parses_l1_rpc_timeout_in_milliseconds() {
        let args = Command::parse_from([
            "base-consensus",
            "--l1-eth-rpc",
            "http://localhost:8545",
            "--l1-beacon",
            "http://localhost:5052",
            "--l1.rpc-timeout-ms",
            "2500",
        ])
        .args;

        assert_eq!(args.l1_rpc_timeout, Duration::from_millis(2500));
    }

    #[test]
    fn finalized_poll_interval_defaults_to_none() {
        let args = Command::parse_from([
            "base-consensus",
            "--l1-eth-rpc",
            "http://localhost:8545",
            "--l1-beacon",
            "http://localhost:5052",
        ])
        .args;

        assert_eq!(args.l1_finalized_poll_interval, None);
    }

    #[test]
    fn parses_finalized_poll_interval_in_milliseconds() {
        let args = Command::parse_from([
            "base-consensus",
            "--l1-eth-rpc",
            "http://localhost:8545",
            "--l1-beacon",
            "http://localhost:5052",
            "--l1.finalized-poll-interval-ms",
            "60000",
        ])
        .args;

        assert_eq!(args.l1_finalized_poll_interval, Some(Duration::from_millis(60000)));
    }
}
