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
/// By default the batch inbox address is sourced from the rollup config fetched
/// at startup via `optimism_rollupConfig`. Shadow deployments may set
/// [`batch_inbox_override`](Self::batch_inbox_override) to submit to a non-canonical inbox.
#[derive(Debug, Clone)]
pub struct BatcherConfig {
    /// L1 RPC endpoint(s).
    ///
    /// One or more HTTP/HTTPS URLs. The service connects to each in order at
    /// startup and uses the first one that responds; later endpoints serve as
    /// startup-time fallbacks only (no per-call rotation). Must be non-empty.
    pub l1_rpc_url: Vec<Url>,
    /// L2 HTTP RPC endpoint(s). Used for all JSON-RPC calls including throttle
    /// control (`miner_setMaxDASize`). Must be HTTP/HTTPS URLs.
    ///
    /// Same connection-time failover semantics as [`l1_rpc_url`](Self::l1_rpc_url):
    /// the service tries each in order and uses the first that connects.
    /// Must be non-empty.
    pub l2_rpc_url: Vec<Url>,
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
    /// rejected without it. Its safe L2 head anchors shadow batcher recovery,
    /// and its derived block hashes are compared with the sequencer.
    pub parity_validator_l2_rpc_url: Option<Url>,
    /// Rollup node RPC endpoint(s).
    ///
    /// Same connection-time failover semantics as [`l1_rpc_url`](Self::l1_rpc_url).
    /// Must be non-empty.
    pub rollup_rpc_url: Vec<Url>,
    /// Signer configuration for signing L1 transactions.
    ///
    /// Must be `Some` before the batcher is started; a `None` value will cause
    /// startup to fail with a clear error rather than proceeding without an L1 identity.
    pub signer: Option<SignerConfig>,
    /// Whether Prometheus metrics are enabled for this service.
    ///
    /// When enabled, the service starts the signer account balance monitor.
    pub metrics_enabled: bool,
    /// Dangerous shadow-mode batch inbox override.
    ///
    /// When set, the batcher still reads the canonical rollup config from the rollup
    /// RPC, but submits L1 transactions to this address instead of
    /// `rollup_config.batch_inbox_address`. This is only intended for explicit
    /// shadow deployments; canonical deployments must leave it unset.
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
            l1_rpc_url: vec!["http://localhost:8545".parse().expect("valid default URL")],
            l1_ws_url: None,
            l2_rpc_url: vec!["http://localhost:9545".parse().expect("valid default URL")],
            parity_validator_l2_rpc_url: None,
            rollup_rpc_url: vec!["http://localhost:7545".parse().expect("valid default URL")],
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
