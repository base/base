//! L1 Client CLI arguments.

use base_consensus_node::L1TxFormat;
use eyre::WrapErr;
use tracing::{info, warn};
use url::Url;

const DEFAULT_L1_TRUST_RPC: bool = true;

/// L1 client arguments.
#[derive(Clone, Debug, clap::Args)]
pub struct L1ClientArgs {
    /// URL of the L1 execution client RPC API.
    #[arg(long, visible_alias = "l1", env = "BASE_NODE_L1_ETH_RPC")]
    pub l1_eth_rpc: Url,
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
    ///
    /// Required for the main Base chain (parent chain = Ethereum), which derives blob-based batches.
    /// Omit it together with `--l1.calldata-only` when the L1 parent chain has no beacon (blob) DA
    /// endpoint - e.g. an appchain settling to Base, which must use calldata (or alt-DA) batching.
    ///
    /// A blank or whitespace-only value is treated as absent (equivalent to not passing the flag),
    /// so an empty env var like `BASE_NODE_L1_BEACON=` works the same as omitting it. Access the
    /// normalized, parsed URL via [`Self::l1_beacon`].
    #[arg(long, visible_alias = "l1.beacon", env = "BASE_NODE_L1_BEACON")]
    pub l1_beacon: Option<String>,
    /// Run derivation without an L1 beacon API (calldata-only / appchain mode).
    ///
    /// Mutually exclusive with `--l1.beacon`. When set, blob-based batches cannot be derived, so the
    /// parent chain must batch via calldata (or alt-DA).
    #[arg(long, visible_alias = "l1.calldata-only", env = "BASE_NODE_L1_CALLDATA_ONLY")]
    pub l1_calldata_only: bool,
    /// The transaction format of the L1 chain: `ethereum` (default) or `base`.
    ///
    /// `base` (an appchain settling to Base) carries deposit (`0x7E`)/EIP-8130 (`0x7D`) txs the
    /// Ethereum envelopes can't deserialize; pair it with `--l1.calldata-only` (no beacon/blob DA).
    #[arg(
        long,
        visible_alias = "l1.tx-format",
        env = "BASE_NODE_L1_TX_FORMAT",
        default_value_t = L1TxFormat::default()
    )]
    pub l1_tx_format: L1TxFormat,
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
    /// Number of L1 blocks to keep distance from the L1 head for the verifier (derivation
    /// pipeline). Controlled via `BASE_NODE_VERIFIER_L1_CONFS`. Defaults to 0, meaning
    /// the verifier derives from the latest L1 head with no confirmation delay.
    #[arg(long = "l1.verifier-confs", default_value = "0", env = "BASE_NODE_VERIFIER_L1_CONFS")]
    pub l1_verifier_confs: u64,
}

impl Default for L1ClientArgs {
    fn default() -> Self {
        Self {
            l1_eth_rpc: Url::parse("http://localhost:8545").unwrap(),
            l1_trust_rpc: DEFAULT_L1_TRUST_RPC,
            l1_beacon: Some("http://localhost:5052".to_string()),
            l1_calldata_only: false,
            l1_tx_format: L1TxFormat::default(),
            l1_slot_duration_override: None,
            l1_verifier_confs: 0,
        }
    }
}

impl L1ClientArgs {
    /// The configured L1 beacon URL, with blank or whitespace-only values treated as absent
    /// (mirroring the prover's `l1_beacon_rpc_from_env`). Returns an error if a non-blank value is
    /// not a valid URL.
    pub fn l1_beacon(&self) -> eyre::Result<Option<Url>> {
        self.l1_beacon
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(|url| Url::parse(url).wrap_err("BASE_NODE_L1_BEACON must be a valid URL"))
            .transpose()
    }

    /// Validates that exactly one derivation mode is selected, making the choice explicit so a
    /// main-chain node cannot silently run without a beacon API.
    ///
    /// - `--l1.beacon <url>` (no `--l1.calldata-only`): beacon mode (main Base chain).
    /// - `--l1.calldata-only` (no `--l1.beacon`): calldata-only mode (appchain).
    /// - neither: error - the operator must state intent.
    /// - both: error - conflicting flags.
    pub fn validate(&self) -> eyre::Result<()> {
        let has_beacon = self.l1_beacon()?.is_some();
        match (has_beacon, self.l1_calldata_only) {
            (true, false) | (false, true) => {}
            (false, false) => {
                return Err(eyre::eyre!(
                    "no L1 derivation mode selected: provide --l1.beacon <url> for the main Base \
                     chain, or pass --l1.calldata-only for an appchain parent with no beacon API"
                ));
            }
            (true, true) => {
                return Err(eyre::eyre!(
                    "conflicting L1 derivation modes: --l1.beacon and --l1.calldata-only are \
                     mutually exclusive"
                ));
            }
        }

        // A Base L1 has no beacon/blob DA, so reject a beacon URL paired with
        // `--l1.tx-format base`.
        if self.l1_tx_format == L1TxFormat::Base && has_beacon {
            return Err(eyre::eyre!(
                "--l1.tx-format base has no beacon/blob DA: do not pass --l1.beacon; run with \
                 --l1.calldata-only"
            ));
        }

        Ok(())
    }

    /// Logs the active L1 derivation mode and L1 transaction format
    /// at startup. Call after [`Self::validate`], which has already vetted the
    /// beacon URL.
    pub fn log_derivation_mode(&self) {
        match self.l1_beacon().ok().flatten() {
            Some(beacon) => {
                info!(target: "rollup_node", l1_tx_format = %self.l1_tx_format, l1_beacon = %beacon, "L1 beacon mode: deriving blob-based batches")
            }
            None => {
                warn!(target: "rollup_node", l1_tx_format = %self.l1_tx_format, "L1 calldata-only mode: no beacon API configured; blob-based batches cannot be derived")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(beacon: Option<&str>, calldata_only: bool) -> L1ClientArgs {
        L1ClientArgs {
            l1_beacon: beacon.map(str::to_string),
            l1_calldata_only: calldata_only,
            ..Default::default()
        }
    }

    #[test]
    fn beacon_mode_is_valid() {
        assert!(args(Some("http://localhost:5052"), false).validate().is_ok());
    }

    #[test]
    fn calldata_only_mode_is_valid() {
        assert!(args(None, true).validate().is_ok());
    }

    #[test]
    fn neither_flag_is_an_error() {
        assert!(args(None, false).validate().is_err());
    }

    #[test]
    fn both_flags_are_an_error() {
        assert!(args(Some("http://localhost:5052"), true).validate().is_err());
    }

    #[test]
    fn base_tx_format_with_beacon_is_an_error() {
        // Base parent has no beacon DA: beacon URL is rejected even though beacon-mode alone is valid.
        let mut a = args(Some("http://localhost:5052"), false);
        a.l1_tx_format = L1TxFormat::Base;
        assert!(a.validate().is_err());
    }

    #[test]
    fn base_tx_format_calldata_only_is_valid() {
        let mut a = args(None, true);
        a.l1_tx_format = L1TxFormat::Base;
        assert!(a.validate().is_ok());
    }

    #[test]
    fn ethereum_tx_format_beacon_mode_is_valid() {
        let mut a = args(Some("http://localhost:5052"), false);
        a.l1_tx_format = L1TxFormat::Ethereum;
        assert!(a.validate().is_ok());
    }

    #[test]
    fn blank_beacon_is_treated_as_missing() {
        // A blank/whitespace beacon is normalized to None, so it routes through the same
        // "no derivation mode selected" path as omitting the flag entirely.
        assert_eq!(args(Some("  "), false).l1_beacon().unwrap(), None);
        assert!(args(Some("  "), false).validate().is_err());
        assert!(args(Some("  "), true).validate().is_ok());
    }

    #[test]
    fn beacon_url_is_trimmed_and_parsed() {
        assert_eq!(
            args(Some(" http://localhost:5052 "), false).l1_beacon().unwrap().unwrap().as_str(),
            "http://localhost:5052/"
        );
    }

    #[test]
    fn malformed_beacon_url_is_an_error() {
        assert!(args(Some("not a url"), false).l1_beacon().is_err());
        assert!(args(Some("not a url"), false).validate().is_err());
    }
}
