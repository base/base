//! L1 Client CLI arguments.

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
    #[arg(long, visible_alias = "l1.beacon", env = "BASE_NODE_L1_BEACON")]
    pub l1_beacon: Option<Url>,
    /// Run derivation without an L1 beacon API (calldata-only / appchain mode).
    ///
    /// Mutually exclusive with `--l1.beacon`. When set, blob-based batches cannot be derived, so the
    /// parent chain must batch via calldata (or alt-DA).
    #[arg(long, visible_alias = "l1.calldata-only", env = "BASE_NODE_L1_CALLDATA_ONLY")]
    pub l1_calldata_only: bool,
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
            l1_beacon: Some(Url::parse("http://localhost:5052").unwrap()),
            l1_calldata_only: false,
            l1_slot_duration_override: None,
            l1_verifier_confs: 0,
        }
    }
}

impl L1ClientArgs {
    /// Validates that exactly one derivation mode is selected, making the choice explicit so a
    /// main-chain node cannot silently run without a beacon API.
    ///
    /// - `--l1.beacon <url>` (no `--l1.calldata-only`): beacon mode (main Base chain).
    /// - `--l1.calldata-only` (no `--l1.beacon`): calldata-only mode (appchain).
    /// - neither: error - the operator must state intent.
    /// - both: error - conflicting flags.
    pub fn validate(&self) -> eyre::Result<()> {
        match (self.l1_beacon.is_some(), self.l1_calldata_only) {
            (true, false) | (false, true) => Ok(()),
            (false, false) => Err(eyre::eyre!(
                "no L1 derivation mode selected: provide --l1.beacon <url> for the main Base chain, \
                 or pass --l1.calldata-only for an appchain parent with no beacon API"
            )),
            (true, true) => Err(eyre::eyre!(
                "conflicting L1 derivation modes: --l1.beacon and --l1.calldata-only are mutually \
                 exclusive"
            )),
        }
    }

    /// Logs the active L1 derivation mode at startup. Call after [`Self::validate`].
    pub fn log_derivation_mode(&self) {
        match self.l1_beacon.as_ref() {
            Some(beacon) => {
                info!(target: "rollup_node", l1_beacon = %beacon, "L1 beacon mode: deriving blob-based batches")
            }
            None => {
                warn!(target: "rollup_node", "L1 calldata-only mode: no beacon API configured; blob-based batches cannot be derived")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(beacon: Option<&str>, calldata_only: bool) -> L1ClientArgs {
        L1ClientArgs {
            l1_beacon: beacon.map(|b| Url::parse(b).unwrap()),
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
}
