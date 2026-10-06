//! Full batcher runtime configuration.

use std::{net::SocketAddr, time::Duration};

use alloy_primitives::Address;
use base_batcher_core::ThrottleConfig;
use base_batcher_encoder::EncoderConfig;
use base_tx_manager::{SignerConfig, TxManagerConfig};
use url::Url;

/// Full batcher configuration combining RPC endpoints, identity, encoding
/// parameters, submission limits, and optional throttling.
///
/// The batch inbox is the one the [`rollup_rpc_url`](Self::rollup_rpc_url) node derives, read at
/// startup through `optimism_rollupConfig`. Shadow deployments point that node at a parity
/// validator deriving a non-canonical inbox and name that inbox in
/// [`batch_inbox_override`](Self::batch_inbox_override).
#[derive(Debug, Clone)]
pub struct BatcherConfig {
    /// L1 RPC endpoint.
    pub l1_rpc_url: Url,
    /// L2 HTTP RPC endpoint, the source of the unsafe blocks the batcher submits. The DA limits
    /// are also pushed to it over `miner_setMaxDASize`.
    pub l2_rpc_url: Url,
    /// Optional L1 WebSocket endpoint for new-block subscriptions.
    ///
    /// When set, the batcher subscribes to new L1 block headers over this
    /// connection to advance the pipeline's L1 head, falling back to polling
    /// [`l1_rpc_url`](Self::l1_rpc_url) only on failure. When absent, polling
    /// is used exclusively.
    pub l1_ws_url: Option<Url>,
    /// Parity validator L2 RPC endpoint for shadow mode.
    ///
    /// Required with [`batch_inbox_override`](Self::batch_inbox_override) and
    /// rejected without it. The validator's derived block hashes are compared
    /// with the sequencer's.
    pub parity_validator_l2_rpc_url: Option<Url>,
    /// Rollup node RPC endpoint.
    ///
    /// The batcher reads the rollup config of this node and follows its derivation, so the
    /// node must derive the inbox the batcher posts to. In shadow mode it is the parity
    /// validator's rollup node.
    pub rollup_rpc_url: Url,
    /// Signer configuration for signing L1 transactions.
    ///
    /// Must be `Some` before the batcher is started; a `None` value will cause
    /// startup to fail with a clear error rather than proceeding without an L1 identity.
    pub signer: Option<SignerConfig>,
    /// Whether Prometheus metrics are enabled for this service.
    ///
    /// When enabled, the service starts the signer account balance monitor.
    pub metrics_enabled: bool,
    /// The shadow inbox, set only in shadow mode.
    ///
    /// Setup refuses to start unless the [`rollup_rpc_url`](Self::rollup_rpc_url) node
    /// derives this inbox, and skips the check that the signer is the `SystemConfig`
    /// batcher. Canonical deployments must leave it unset.
    pub batch_inbox_override: Option<Address>,
    /// L2 block polling interval.
    pub poll_interval: Duration,
    /// Encoder configuration.
    pub encoder_config: EncoderConfig,
    /// Maximum number of in-flight (unconfirmed) transactions.
    pub max_pending_transactions: usize,
    /// Transaction manager configuration.
    pub tx_manager: TxManagerConfig,
    /// Throttle configuration (optional).
    pub throttle: Option<ThrottleConfig>,
    /// Number of recent L1 blocks to inspect for a confirmed batcher transaction.
    ///
    /// When [`wait_node_sync`](Self::wait_node_sync) is enabled, recent batcher
    /// account nonce activity selects the L1 synchronization target in this window.
    /// This never changes the L2 backfill cursor.
    ///
    /// Must be zero unless [`wait_node_sync`](Self::wait_node_sync) is enabled.
    /// Must be at most [`MAX_CHECK_RECENT_TXS_DEPTH`](crate::MAX_CHECK_RECENT_TXS_DEPTH)
    /// (128). A value of 0 disables the scan (default).
    pub check_recent_txs_depth: u64,
    /// Socket address for the admin JSON-RPC API.
    ///
    /// When set, the batcher exposes the `admin_*` RPC namespace on this address.
    /// When `None` (the default), the admin server is disabled.
    pub admin_addr: Option<SocketAddr>,
    /// If `true`, start in a stopped state and defer batch submission until
    /// `admin_startBatcher` is called via the admin API.
    pub stopped: bool,
    /// If `true`, block startup until the rollup node has processed the selected
    /// L1 synchronization target.
    ///
    /// Useful when the batcher is started before the node has finished its
    /// initial sync — without this gate the initial backfill would race the
    /// node's derivation pipeline and could submit redundant data.
    pub wait_node_sync: bool,
    /// Budget for retrying one-shot startup RPCs, and the maximum time to wait
    /// for the rollup node to report sync when [`wait_node_sync`](Self::wait_node_sync)
    /// is set.
    ///
    /// On expiry the service exits with an error rather than hanging
    /// indefinitely, giving operators a clear signal that the upstream node is
    /// misconfigured or unreachable. Default: 10 minutes.
    pub wait_node_sync_timeout: Duration,
    /// When `true` and DA-backlog throttling is active, force the encoder to
    /// emit blob-typed submissions even when its configured `da_type` is
    /// calldata. No-op for blob-configured batchers. Default: `true`.
    pub force_blobs_when_throttling: bool,
}

impl Default for BatcherConfig {
    fn default() -> Self {
        Self {
            l1_rpc_url: "http://localhost:8545".parse().expect("valid default URL"),
            l1_ws_url: None,
            l2_rpc_url: "http://localhost:9545".parse().expect("valid default URL"),
            parity_validator_l2_rpc_url: None,
            rollup_rpc_url: "http://localhost:7545".parse().expect("valid default URL"),
            signer: None,
            metrics_enabled: false,
            batch_inbox_override: None,
            poll_interval: Duration::from_secs(1),
            encoder_config: EncoderConfig::default(),
            max_pending_transactions: 1,
            tx_manager: TxManagerConfig { num_confirmations: 1, ..TxManagerConfig::default() },
            throttle: Some(ThrottleConfig::default()),
            check_recent_txs_depth: 0,
            admin_addr: None,
            stopped: false,
            wait_node_sync: false,
            wait_node_sync_timeout: Duration::from_secs(600),
            force_blobs_when_throttling: true,
        }
    }
}

impl BatcherConfig {
    /// Returns the inbox the batcher posts to, [`batch_inbox_override`](Self::batch_inbox_override)
    /// when set and otherwise `derived_inbox`, the inbox the rollup node derives.
    ///
    /// # Errors
    ///
    /// Returns an error when the override differs from `derived_inbox`, because the batcher
    /// follows the derivation of the [`rollup_rpc_url`](Self::rollup_rpc_url) node.
    pub fn batch_inbox(&self, derived_inbox: Address) -> eyre::Result<Address> {
        match self.batch_inbox_override {
            Some(inbox) if inbox != derived_inbox => eyre::bail!(
                "the rollup node derives inbox {derived_inbox} instead of the shadow inbox \
                 {inbox}, point the rollup RPC at the parity validator's rollup node"
            ),
            Some(inbox) => Ok(inbox),
            None => Ok(derived_inbox),
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    const CANONICAL_INBOX: Address = Address::repeat_byte(0xca);
    const SHADOW_INBOX: Address = Address::repeat_byte(0x5a);

    #[rstest]
    #[case::canonical(None, CANONICAL_INBOX)]
    #[case::shadow(Some(SHADOW_INBOX), SHADOW_INBOX)]
    fn batch_inbox_is_the_inbox_the_rollup_node_derives(
        #[case] batch_inbox_override: Option<Address>,
        #[case] derived_inbox: Address,
    ) {
        let config = BatcherConfig { batch_inbox_override, ..BatcherConfig::default() };

        assert_eq!(config.batch_inbox(derived_inbox).unwrap(), derived_inbox);
    }

    #[test]
    fn batch_inbox_rejects_a_rollup_node_that_derives_another_inbox() {
        let config =
            BatcherConfig { batch_inbox_override: Some(SHADOW_INBOX), ..BatcherConfig::default() };

        let error = config.batch_inbox(CANONICAL_INBOX).unwrap_err();

        assert_eq!(
            error.to_string(),
            format!(
                "the rollup node derives inbox {CANONICAL_INBOX} instead of the shadow inbox \
                 {SHADOW_INBOX}, point the rollup RPC at the parity validator's rollup node"
            )
        );
    }
}
