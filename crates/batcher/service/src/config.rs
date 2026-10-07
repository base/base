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
/// The batcher posts to the batch inbox of the rollup config of the node it follows, read at
/// startup through `optimism_rollupConfig`: the leader sequencer's rollup node, or in shadow mode
/// the parity validator of [`shadow`](Self::shadow).
#[derive(Debug, Clone)]
pub struct BatcherConfig {
    /// L1 HTTP RPC endpoint.
    pub l1_rpc_url: Url,
    /// Sequencer HTTP endpoints, at least one: conductors with their RPC proxy enabled, or
    /// consensus nodes that forward the methods they do not serve to their execution client.
    ///
    /// The batcher reads the unsafe blocks of the leader and, outside shadow mode, follows its
    /// derivation. Among several endpoints, the leader is the first whose `admin_sequencerActive`
    /// answers `true`. The DA limits are pushed to every endpoint when the
    /// [`throttle`](Self::throttle) is on.
    pub sequencer_urls: Vec<Url>,
    /// Optional L1 WebSocket endpoint for new-block subscriptions.
    ///
    /// When set, the batcher also takes new L1 heads from a subscription over this connection,
    /// alongside polling [`l1_rpc_url`](Self::l1_rpc_url). A connection or subscription that
    /// fails or outlasts [`network_timeout`](Self::network_timeout) leaves polling alone. When
    /// absent, polling is used exclusively.
    pub l1_ws_url: Option<Url>,
    /// Signer configuration for signing L1 transactions.
    ///
    /// Must be `Some` before the batcher is started; a `None` value will cause
    /// startup to fail with a clear error rather than proceeding without an L1 identity.
    pub signer: Option<SignerConfig>,
    /// Whether Prometheus metrics are enabled for this service.
    ///
    /// When enabled, the service starts the signer account balance monitor.
    pub metrics_enabled: bool,
    /// Shadow mode settings, `None` for a canonical batcher.
    pub shadow: Option<ShadowConfig>,
    /// Polling interval.
    pub poll_interval: Duration,
    /// Timeout of the RPC calls to L1, the sequencers, the parity validator and the block
    /// builders.
    pub network_timeout: Duration,
    /// Encoder configuration.
    pub encoder_config: EncoderConfig,
    /// Maximum number of in-flight (unconfirmed) transactions.
    pub max_pending_transactions: usize,
    /// Transaction manager configuration. Its `network_timeout` is replaced by
    /// [`network_timeout`](Self::network_timeout).
    pub tx_manager: TxManagerConfig,
    /// DA throttle configuration, `None` to disable the throttle.
    ///
    /// Must be `None` when [`shadow`](Self::shadow) is set.
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
            sequencer_urls: vec!["http://localhost:7545".parse().expect("valid default URL")],
            signer: None,
            metrics_enabled: false,
            shadow: None,
            poll_interval: Duration::from_secs(1),
            network_timeout: Duration::from_secs(10),
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

/// The settings of a shadow batcher, which posts to a non-canonical inbox that a parity
/// validator derives its chain from.
#[derive(Debug, Clone)]
pub struct ShadowConfig {
    /// The shadow inbox, which must be the batch inbox of the parity validator's rollup config.
    pub inbox: Address,
    /// Rollup node RPC endpoint of the parity validator, whose rollup config the batcher reads
    /// and whose derivation it follows.
    pub validator_rollup_rpc: Url,
    /// L2 HTTP RPC endpoint of the parity validator, whose derived block hashes are compared
    /// with the leader sequencer's.
    pub validator_l2_rpc: Url,
}

impl ShadowConfig {
    /// Checks that `batch_inbox`, the batch inbox of the parity validator's rollup config, is
    /// the shadow [`inbox`](Self::inbox).
    ///
    /// # Errors
    ///
    /// Returns an error when it is another inbox, because the batcher posts to the batch inbox
    /// of that config.
    pub fn validate_batch_inbox(&self, batch_inbox: Address) -> eyre::Result<()> {
        if batch_inbox != self.inbox {
            eyre::bail!(
                "the batch inbox of the parity validator's rollup config is {batch_inbox} \
                 instead of the shadow inbox {inbox}, check --shadow.inbox and \
                 --shadow.validator-rollup-rpc",
                inbox = self.inbox
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANONICAL_INBOX: Address = Address::repeat_byte(0xca);
    const SHADOW_INBOX: Address = Address::repeat_byte(0x5a);

    /// A shadow config accepts the shadow inbox as the batch inbox of the parity validator's
    /// rollup config, and refuses another one.
    #[test]
    fn validate_batch_inbox_requires_the_shadow_inbox() {
        let shadow = ShadowConfig {
            inbox: SHADOW_INBOX,
            validator_rollup_rpc: "http://localhost:7545".parse().unwrap(),
            validator_l2_rpc: "http://localhost:8545".parse().unwrap(),
        };

        shadow.validate_batch_inbox(SHADOW_INBOX).unwrap();

        let error = shadow.validate_batch_inbox(CANONICAL_INBOX).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "the batch inbox of the parity validator's rollup config is {CANONICAL_INBOX} \
                 instead of the shadow inbox {SHADOW_INBOX}, check --shadow.inbox and \
                 --shadow.validator-rollup-rpc"
            )
        );
    }
}
