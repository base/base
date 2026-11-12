//! Core transaction manager implementation.
//!
//! [`SimpleTxManager`] takes a [`TxCandidate`] through the full construction
//! pipeline: gas price estimation via [`suggest_gas_price_caps`], optional fee
//! override enforcement, fee limit checks via [`FeeCalculator::check_limits`],
//! gas estimation or validation against the provider, nonce assignment via
//! [`NonceManager`], and signing via alloy's [`NetworkWallet`] trait.
//!
//! The outer [`prepare`] method wraps [`craft_tx`] in a `backon` retry loop
//! (up to 30 attempts, 2-second fixed delay) that retries only on transient
//! errors and exits immediately on shutdown. Both methods accept optional fee
//! overrides and return a [`PreparedTx`] containing the raw transaction bytes
//! and the actual fees that were applied, eliminating the need for callers to
//! re-query gas prices after transaction construction.
//!
//! Blob transactions are priced by [`BlobFeePolicy`] instead of the provider's
//! suggested priority fee, and skip the fee limit check: the policy's upper bound
//! limits their priority fee instead.
//!
//! The [`send_tx`] method drives a signed transaction from publication through
//! mempool to onchain confirmation via a `tokio::select!` event loop that
//! coordinates fee bumping, receipt polling, and critical error detection.
//! The `select!` block only determines which event fired; fee bump logic
//! (including [`prepare`]) always runs to completion outside the `select!`
//! block to preserve cancellation safety.
//!
//! All transaction fields are set manually on [`TransactionRequest`] — no
//! alloy fillers or `PendingTransactionBuilder` are used.
//!
//! [`TxCandidate`]: crate::TxCandidate
//! [`BlobFeePolicy`]: crate::BlobFeePolicy
//! [`suggest_gas_price_caps`]: SimpleTxManager::suggest_gas_price_caps
//! [`FeeCalculator::check_limits`]: crate::FeeCalculator::check_limits
//! [`NonceManager`]: crate::NonceManager
//! [`NetworkWallet`]: alloy_network::NetworkWallet
//! [`prepare`]: SimpleTxManager::prepare
//! [`craft_tx`]: SimpleTxManager::craft_tx
//! [`send_tx`]: SimpleTxManager::send_tx
//! [`TransactionRequest`]: alloy_rpc_types_eth::TransactionRequest

use std::{fmt::Debug, sync::Arc, time::Duration};

use alloy_consensus::TxEnvelope;
use alloy_eips::{
    BlockNumberOrTag, Decodable2718, Encodable2718, eip7594::BlobTransactionSidecarEip7594,
};
use alloy_network::{
    Ethereum, EthereumWallet, Network, NetworkTransactionBuilder, NetworkWallet,
    TransactionBuilder, TransactionBuilderError,
};
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::{Block, TransactionReceipt, TransactionRequest};
use alloy_transport::TransportError;
use backon::{ConstantBuilder, Retryable};
use base_runtime::{Runtime, RuntimeTimeout, TokioRuntime};
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    BlobFeePolicy, BlobTxBuilder, BumpedFees, FeeCalculator, FeeOverride, GasPriceCaps,
    NonceManager, RpcErrorClassifier, SendHandle, SendOutcome, SendResponse, SendState,
    SignerConfig, TxCandidate, TxManager, TxManagerConfig, TxManagerError, TxManagerResult,
    TxMetrics,
};

/// Number of wei in one gwei (10^9), as `f64` for fractional-precision
/// metric conversions that must not truncate sub-gwei values.
///
/// `u128 as f64` is lossless for values up to 2^53 (~9 × 10^15 wei, or
/// ~9 million gwei). Realistic per-gas fee values (tip cap, base fee) are
/// well under this threshold, so the conversion preserves full precision.
const WEI_PER_GWEI: f64 = 1_000_000_000.0;

/// A signed transaction together with the fee values that were applied
/// during construction.
///
/// Returned by [`SimpleTxManager::prepare`] and
/// [`SimpleTxManager::craft_tx`] so that callers can track the actual
/// on-wire fees without a redundant gas price query.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct PreparedTx {
    /// RLP-encoded signed transaction bytes.
    pub raw_tx: Bytes,
    /// Maximum priority fee per gas (tip) used in the signed transaction.
    pub gas_tip_cap: u128,
    /// Maximum total fee per gas used in the signed transaction.
    pub gas_fee_cap: u128,
    /// Blob fee cap used in the signed transaction. `None` for non-blob txs.
    pub blob_fee_cap: Option<u128>,
    /// Gas limit used in the signed transaction.
    pub gas_limit: u64,
    /// Nonce assigned to the signed transaction.
    pub nonce: u64,
    /// Cached blob sidecar from transaction construction. `None` for non-blob txs.
    ///
    /// Wrapped in `Arc` so cloning across fee-bump iterations is a pointer
    /// copy rather than a deep clone (~800 KB per blob). One deep clone per
    /// `craft_tx` call is unavoidable because alloy's
    /// `TransactionRequest::sidecar` requires an owned value.
    pub sidecar: Option<Arc<BlobTransactionSidecarEip7594>>,
}

impl PreparedTx {
    /// Decodes the signed transaction bytes into a [`TxEnvelope`].
    pub fn to_envelope(&self) -> Result<TxEnvelope, alloy_eips::eip2718::Eip2718Error> {
        TxEnvelope::decode_2718(&mut self.raw_tx.as_ref())
    }
}

/// A receipt confirmed to the required depth, with the timestamp of the canonical
/// block that includes it.
///
/// The receipt poller already reads that block to check the receipt is canonical, so
/// the send loop gets the inclusion time without another RPC call.
#[derive(Debug)]
pub struct ConfirmedTx {
    /// The confirmed receipt.
    pub receipt: TransactionReceipt,
    /// Unix timestamp, in seconds, of the block that includes the transaction.
    pub block_timestamp: u64,
}

/// Mutable fee-bump state tracked across iterations in the send loop.
///
/// Groups the values that [`SimpleTxManager::try_fee_bump`] updates on each
/// successful fee bump so they can be passed as a single argument.
#[derive(Debug)]
struct BumpState {
    /// Most recently published transaction, whose fees the next bump starts from.
    prepared: PreparedTx,
    /// Hash of the most recently published transaction.
    tx_hash: B256,
    /// Hash and `max_priority_fee_per_gas` of every published version, any of
    /// which may land.
    published: Vec<(B256, u128)>,
}

impl BumpState {
    /// Constructs a [`BumpState`] from the first published version.
    fn new(prepared: PreparedTx, tx_hash: B256) -> Self {
        Self { published: vec![(tx_hash, prepared.gas_tip_cap)], prepared, tx_hash }
    }

    /// Records a published replacement.
    fn replace(&mut self, prepared: PreparedTx, tx_hash: B256) {
        self.published.push((tx_hash, prepared.gas_tip_cap));
        self.prepared = prepared;
        self.tx_hash = tx_hash;
    }
}

/// Default transaction manager implementation.
///
/// Constructs, signs, and submits EIP-1559 transactions. All RPC fields
/// (nonce, gas, fees) are set manually on [`TransactionRequest`] without
/// alloy fillers or `PendingTransactionBuilder`.
#[derive(Debug, Clone)]
pub struct SimpleTxManager<P, R = TokioRuntime> {
    /// RPC provider for chain queries and transaction submission.
    provider: P,
    /// Runtime used for timers and spawned background tasks.
    runtime: R,
    /// Wallet used for signing transactions.
    wallet: EthereumWallet,
    /// Validated runtime configuration.
    config: TxManagerConfig,
    /// Nonce manager for sequential nonce allocation.
    nonce_manager: NonceManager<P, R>,
    /// Chain ID for transaction construction.
    chain_id: u64,
    /// Shutdown signal shared by clones of the manager and its spawned
    /// background tasks: [`close`](Self::close) cancels it, which both sets
    /// the flag read by [`is_closed`](Self::is_closed) and wakes in-flight
    /// sends, including those spawned via [`send_async`](Self::send_async)
    /// and [`wait_for_tx`](Self::wait_for_tx).
    closed: CancellationToken,
    /// Metrics collector for transaction lifecycle events.
    metrics: Arc<dyn TxMetrics>,
    /// Prices blob transactions. Shared by clones of the manager, so each blob
    /// transaction starts from the fees of the previous ones.
    blob_fee_policy: BlobFeePolicy,
}

impl<P> SimpleTxManager<P, TokioRuntime>
where
    P: Provider + Clone + Debug + Send + Sync + 'static,
{
    /// Creates a new [`SimpleTxManager`] from a [`SignerConfig`].
    ///
    /// Builds the [`EthereumWallet`] from `signer_config`, then delegates
    /// to [`from_wallet`](Self::from_wallet).
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::WalletConstruction`] if the wallet cannot
    /// be built. See [`from_wallet`](Self::from_wallet) for other error
    /// conditions.
    pub async fn new(
        provider: P,
        signer_config: SignerConfig,
        config: TxManagerConfig,
        chain_id: u64,
        metrics: Arc<dyn TxMetrics>,
    ) -> TxManagerResult<Self> {
        let wallet = signer_config.build_wallet()?;
        Self::from_wallet(provider, wallet, config, chain_id, metrics).await
    }

    /// Creates a new [`SimpleTxManager`] from a pre-built [`EthereumWallet`].
    ///
    /// This is the primary constructor used by tests and callers that need
    /// custom signers (e.g. `FailingSigner`). Internally creates a
    /// [`NonceManager`] using the wallet's default signer address and the
    /// config's `network_timeout`. Fetches the chain ID from the provider
    /// and validates it against the supplied `chain_id` to prevent
    /// constructing transactions with the wrong chain ID.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::InvalidConfig`] if config validation
    /// fails or the chain ID does not match the provider. Returns
    /// [`TxManagerError::Rpc`] if the provider is unreachable.
    pub async fn from_wallet(
        provider: P,
        wallet: EthereumWallet,
        config: TxManagerConfig,
        chain_id: u64,
        metrics: Arc<dyn TxMetrics>,
    ) -> TxManagerResult<Self> {
        Self::from_wallet_with_runtime(
            TokioRuntime::new(),
            provider,
            wallet,
            config,
            chain_id,
            metrics,
        )
        .await
    }
}

impl<P, R> SimpleTxManager<P, R>
where
    P: Provider + Clone + Debug + Send + Sync + 'static,
    R: Runtime,
{
    /// Maximum number of retry attempts for [`Self::prepare`].
    pub const PREPARE_MAX_RETRIES: usize = 30;

    /// Fixed delay between retry attempts in [`Self::prepare`].
    pub const PREPARE_RETRY_DELAY: Duration = Duration::from_secs(2);

    /// Creates a new [`SimpleTxManager`] with an injected runtime.
    ///
    /// This mirrors [`Self::new`] but lets deterministic tests control
    /// timers and spawned tasks through `base-runtime`.
    pub async fn new_with_runtime(
        runtime: R,
        provider: P,
        signer_config: SignerConfig,
        config: TxManagerConfig,
        chain_id: u64,
        metrics: Arc<dyn TxMetrics>,
    ) -> TxManagerResult<Self> {
        let wallet = signer_config.build_wallet()?;
        Self::from_wallet_with_runtime(runtime, provider, wallet, config, chain_id, metrics).await
    }

    /// Creates a new [`SimpleTxManager`] from a pre-built wallet and runtime.
    ///
    /// See [`Self::from_wallet`] for the production Tokio-backed constructor.
    pub async fn from_wallet_with_runtime(
        runtime: R,
        provider: P,
        wallet: EthereumWallet,
        config: TxManagerConfig,
        chain_id: u64,
        metrics: Arc<dyn TxMetrics>,
    ) -> TxManagerResult<Self> {
        config.validate().map_err(|e| TxManagerError::InvalidConfig(e.to_string()))?;

        // Cross-validate chain_id against the provider to catch
        // misconfiguration early rather than failing at tx submission.
        let provider_chain_id =
            RuntimeTimeout::run(&runtime, config.network_timeout, provider.get_chain_id())
                .await
                .map_err(|_| TxManagerError::Rpc("get_chain_id timed out".into()))?
                .map_err(|e| RpcErrorClassifier::classify_rpc_error(&e))?;

        if chain_id != provider_chain_id {
            return Err(TxManagerError::InvalidConfig(format!(
                "chain_id mismatch: supplied {chain_id}, provider returned {provider_chain_id}"
            )));
        }

        let address = <EthereumWallet as NetworkWallet<Ethereum>>::default_signer_address(&wallet);
        let nonce_manager = NonceManager::with_runtime(
            runtime.clone(),
            provider.clone(),
            address,
            config.network_timeout,
        );
        let blob_fee_policy = BlobFeePolicy::new(
            config.max_priority_fee_per_gas_per_blob_lower_bound,
            config.max_priority_fee_per_gas_per_blob_upper_bound,
        );
        Ok(Self {
            provider,
            runtime,
            wallet,
            config,
            nonce_manager,
            chain_id,
            closed: CancellationToken::new(),
            metrics,
            blob_fee_policy,
        })
    }

    /// Uses the `pending` block tag for nonce initialization and after nonce resets.
    pub fn with_pending_nonce(mut self) -> Self {
        self.nonce_manager = self.nonce_manager.with_pending_tag();
        self
    }

    /// Returns a reference to the RPC provider.
    pub const fn provider(&self) -> &P {
        &self.provider
    }

    /// Returns a reference to the wallet.
    pub const fn wallet(&self) -> &EthereumWallet {
        &self.wallet
    }

    /// Returns a reference to the configuration.
    pub const fn config(&self) -> &TxManagerConfig {
        &self.config
    }

    /// Returns a reference to the nonce manager.
    pub const fn nonce_manager(&self) -> &NonceManager<P, R> {
        &self.nonce_manager
    }

    /// Returns the chain ID.
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Returns a reference to the metrics collector.
    pub fn metrics(&self) -> &dyn TxMetrics {
        self.metrics.as_ref()
    }

    /// Signals the manager to stop accepting new transactions.
    ///
    /// After calling `close()`, any subsequent call to [`prepare`](Self::prepare)
    /// will immediately return `Err(TxManagerError::ChannelClosed)`.
    /// In-flight sends waiting for a receipt or the next fee bump return
    /// `Err(TxManagerError::ChannelClosed)` immediately; a fee bump already in
    /// progress runs to completion first, because preparation is not
    /// cancellation-safe. A transaction that was already published may still
    /// be mined after the send returns.
    /// Background tasks spawned by [`send_async`](Self::send_async) and
    /// receipt polling tasks will also observe the shutdown and exit
    /// gracefully, since they share the manager's cancellation token.
    pub fn close(&self) {
        self.closed.cancel();
    }

    /// Returns `true` if the manager has been closed via [`close`](Self::close).
    pub fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }

    /// Records an RPC error metric and returns a [`TxManagerError::Rpc`].
    fn rpc_error(&self, msg: &str) -> TxManagerError {
        self.metrics.record_rpc_error();
        TxManagerError::Rpc(msg.into())
    }

    /// Classifies a [`TransportError`] and records an RPC error metric when
    /// the error is an unrecognised transport failure ([`TxManagerError::Rpc`]).
    /// Recognised state errors (e.g. `NonceTooLow`, `ExecutionReverted`) are
    /// returned without inflating the RPC error counter.
    fn classify_and_record_rpc(&self, error: &TransportError) -> TxManagerError {
        let err = RpcErrorClassifier::classify_rpc_error(error);
        if err.is_rpc_error() {
            self.metrics.record_rpc_error();
        }
        err
    }

    /// Checks that `fee` does not exceed the configured ceiling relative to
    /// `suggested` (the raw provider estimate before enforced minimums).
    const fn check_fee_limit(&self, fee: u128, suggested: u128) -> TxManagerResult<()> {
        FeeCalculator::check_limits(
            fee,
            suggested,
            self.config.fee_limit_multiplier,
            self.config.fee_limit_threshold,
        )
    }

    /// Constructs and signs a transaction, retrying on transient errors.
    ///
    /// Wraps [`craft_tx`](Self::craft_tx) in a retry loop with up to
    /// [`Self::PREPARE_MAX_RETRIES`] attempts and a [`Self::PREPARE_RETRY_DELAY`]
    /// fixed delay between retries. Only errors where
    /// [`TxManagerError::is_retryable`] returns `true` trigger a retry.
    ///
    /// When `fee_overrides` is `Some`, the provided [`FeeOverride`] values
    /// are used as a floor — `craft_tx` takes `max(network_fee, override)`
    /// so the resulting transaction is guaranteed to meet the override
    /// thresholds. Pass `None` for the initial send; pass the bumped fees
    /// during fee-bump iterations to ensure replacement transactions
    /// satisfy geth's replacement rules. For a blob transaction, the network
    /// fees are those of [`BlobFeePolicy::fees`].
    ///
    /// # Cancellation safety
    ///
    /// This future is **not** cancellation-safe. Each retry iteration
    /// delegates to [`craft_tx`](Self::craft_tx), which acquires a nonce
    /// from the [`NonceManager`]. If the future is dropped after nonce
    /// acquisition but before signing completes, the nonce is consumed
    /// without producing a transaction, creating a permanent nonce gap.
    /// Callers must not wrap this future in `tokio::select!`,
    /// `tokio::time::timeout`, or similar combinators that may drop it
    /// mid-flight.
    ///
    /// # Errors
    ///
    /// Returns immediately with [`TxManagerError::ChannelClosed`] if the
    /// manager is closed. Otherwise returns the first non-retryable error,
    /// or the last retryable error after exhausting all retry attempts.
    pub async fn prepare(
        &self,
        candidate: &TxCandidate,
        fee_overrides: Option<FeeOverride>,
    ) -> TxManagerResult<PreparedTx> {
        self.prepare_with_initial_caps(candidate, fee_overrides, None, None, None).await
    }

    /// Internal variant of [`prepare`](Self::prepare) that optionally reuses
    /// caller-supplied fee caps on the first attempt only.
    ///
    /// This is used by the fee-bump path to avoid a redundant
    /// `suggest_gas_price_caps()` round-trip after it has already fetched caps
    /// to compute bump thresholds. Retries intentionally fall back to fresh fee
    /// estimation so transient failures do not pin stale network conditions.
    ///
    /// When `nonce_override` is `Some(n)`, the same pre-assigned nonce is
    /// forwarded to every retry attempt (the nonce was already consumed from
    /// the nonce manager, so it must be reused on retries).
    async fn prepare_with_initial_caps(
        &self,
        candidate: &TxCandidate,
        fee_overrides: Option<FeeOverride>,
        mut initial_caps: Option<GasPriceCaps>,
        nonce_override: Option<u64>,
        cached_sidecar: Option<Arc<BlobTransactionSidecarEip7594>>,
    ) -> TxManagerResult<PreparedTx> {
        (|| {
            let caps = initial_caps.take();
            let sidecar = cached_sidecar.clone();
            async move {
                // Re-check closed flag on each retry attempt to avoid wasted
                // RPC calls after shutdown. ChannelClosed is non-retryable,
                // so backon exits the loop immediately.
                if self.is_closed() {
                    return Err(TxManagerError::ChannelClosed);
                }
                self.craft_tx_with_caps(candidate, fee_overrides, caps, nonce_override, sidecar)
                    .await
            }
        })
        .retry(
            ConstantBuilder::default()
                .with_delay(Self::PREPARE_RETRY_DELAY)
                .with_max_times(Self::PREPARE_MAX_RETRIES),
        )
        .when(|e: &TxManagerError| e.is_retryable())
        .notify(|err, dur| {
            warn!(error = %err, delay = ?dur, "retrying craft_tx");
        })
        .await
    }

    /// Queries the provider for current gas price estimates of a regular
    /// (non-blob) transaction.
    ///
    /// Returns a [`GasPriceCaps`] containing:
    /// - `gas_tip_cap`: maximum priority fee (enforced >= `config.min_tip_cap`)
    /// - `gas_fee_cap`: `tip + 2 * base_fee` via [`FeeCalculator::calc_gas_fee_cap`],
    ///   with the base fee enforced >= `config.min_basefee`
    /// - `raw_gas_fee_cap`: fee cap from the raw provider values before
    ///   enforcing minimums, used as the `suggested` baseline in
    ///   [`FeeCalculator::check_limits`]
    ///
    /// Blob transactions are priced by [`BlobFeePolicy`] instead, inside
    /// [`craft_tx`](Self::craft_tx) and [`prepare`](Self::prepare).
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::Rpc`] if any provider call fails.
    pub async fn suggest_gas_price_caps(&self) -> TxManagerResult<GasPriceCaps> {
        // Query tip cap and latest block concurrently.
        let (tip_result, block_result) = tokio::join!(
            RuntimeTimeout::run(
                &self.runtime,
                self.config.network_timeout,
                self.provider.get_max_priority_fee_per_gas(),
            ),
            self.latest_block(),
        );

        let raw_tip_cap = tip_result
            .map_err(|_| self.rpc_error("get_max_priority_fee_per_gas timed out"))?
            .map_err(|e| self.classify_and_record_rpc(&e))?;
        let raw_base_fee = self.base_fee(block_result?)?;

        // Compute raw gas fee cap from provider values before enforcing minimums.
        let raw_gas_fee_cap = FeeCalculator::calc_gas_fee_cap(raw_base_fee, raw_tip_cap);

        // Enforce minimum tip cap and base fee.
        let tip_cap = raw_tip_cap.max(self.config.min_tip_cap);
        let base_fee = raw_base_fee.max(self.config.min_basefee);

        // Compute gas fee cap with enforced minimums.
        let gas_fee_cap = FeeCalculator::calc_gas_fee_cap(base_fee, tip_cap);

        // Record fee metrics (converted from wei to gwei with fractional precision).
        self.metrics.record_basefee(base_fee as f64 / WEI_PER_GWEI);
        self.metrics.record_tipcap(tip_cap as f64 / WEI_PER_GWEI);

        Ok(GasPriceCaps { gas_tip_cap: tip_cap, gas_fee_cap, raw_gas_fee_cap })
    }

    /// Queries the provider for the base fee and the blob base fee that
    /// [`BlobFeePolicy`] prices blob transactions from.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::Rpc`] if any provider call fails.
    async fn blob_base_fees(&self) -> TxManagerResult<(u128, u128)> {
        let (block_result, blob_base_fee_result) = tokio::join!(
            self.latest_block(),
            RuntimeTimeout::run(
                &self.runtime,
                self.config.network_timeout,
                self.provider.get_blob_base_fee(),
            ),
        );
        let base_fee = self.base_fee(block_result?)?;
        let blob_base_fee = blob_base_fee_result
            .map_err(|_| self.rpc_error("get_blob_base_fee timed out"))?
            .map_err(|e| self.classify_and_record_rpc(&e))?;
        self.metrics.record_basefee(base_fee as f64 / WEI_PER_GWEI);
        Ok((base_fee, blob_base_fee))
    }

    /// Fetches the latest block.
    async fn latest_block(&self) -> TxManagerResult<Option<Block>> {
        RuntimeTimeout::run(
            &self.runtime,
            self.config.network_timeout,
            self.provider.get_block_by_number(BlockNumberOrTag::Latest),
        )
        .await
        .map_err(|_| self.rpc_error("get_block_by_number timed out"))?
        .map_err(|e| self.classify_and_record_rpc(&e))
    }

    /// Returns the base fee of the latest block.
    fn base_fee(&self, latest_block: Option<Block>) -> TxManagerResult<u128> {
        latest_block
            .ok_or_else(|| self.rpc_error("latest block not found"))?
            .header
            .base_fee_per_gas
            .map(u128::from)
            .ok_or_else(|| self.rpc_error("base fee not available"))
    }

    /// Computes bumped fee parameters for a replacement of a regular (non-blob)
    /// transaction.
    ///
    /// Given the previous on-wire fees, applies
    /// [`FeeCalculator::update_fees`] to satisfy geth's tx-replacement
    /// rules and enforces the configured fee ceiling via
    /// [`FeeCalculator::check_limits`]. Blob transactions are replaced with
    /// [`BlobFeePolicy::replacement`] instead.
    ///
    /// The returned [`BumpedFees`] carries the fresh [`GasPriceCaps`] so
    /// callers can forward them to the internal preparation path without a
    /// redundant provider round-trip. Gas estimation is
    /// intentionally **not** performed here — callers should rely on
    /// [`craft_tx`](Self::craft_tx), which estimates gas as part of
    /// transaction construction.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::FeeLimitExceeded`] when the bumped fee
    /// cap exceeds the configured ceiling, or an RPC error if fee
    /// estimation fails.
    pub async fn increase_gas_price(
        &self,
        old_tip: u128,
        old_fee_cap: u128,
    ) -> TxManagerResult<BumpedFees> {
        // Step 1: Fetch fresh network fees.
        let caps = self.suggest_gas_price_caps().await?;

        // Step 2: Derive effective base fee from the caps.
        let new_base_fee = FeeCalculator::base_fee_from_caps(caps.gas_fee_cap, caps.gas_tip_cap);

        // Step 3: Compute bumped tip and fee cap.
        let (bumped_tip, bumped_fee_cap) =
            FeeCalculator::update_fees(old_tip, old_fee_cap, caps.gas_tip_cap, new_base_fee, false);

        // Step 4: Enforce the fee ceiling.
        self.check_fee_limit(bumped_fee_cap, caps.raw_gas_fee_cap)?;

        info!(%old_tip, %old_fee_cap, %bumped_tip, %bumped_fee_cap, "gas price increase computed");

        Ok(BumpedFees { gas_tip_cap: bumped_tip, gas_fee_cap: bumped_fee_cap, caps })
    }

    /// Constructs and signs a single transaction from a [`TxCandidate`].
    ///
    /// Steps:
    /// 1. Query gas price caps via [`suggest_gas_price_caps`](Self::suggest_gas_price_caps)
    /// 2. Apply fee overrides as a floor (`max(network_fee, override)`)
    /// 3. Check fee limits via [`FeeCalculator::check_limits`]
    ///
    ///    A blob transaction is priced by [`BlobFeePolicy::fees`] from the
    ///    current base fee and blob base fee instead, with the fee overrides
    ///    as a floor up to the policy's upper bound.
    /// 4. Build a [`TransactionRequest`] with all fields set manually
    /// 5. Estimate gas via the provider; use `max(estimate, candidate.gas_limit)` as floor
    /// 6. Assign nonce via [`NonceManager::next_nonce`]
    /// 7. Sign and RLP-encode to raw transaction bytes
    ///
    /// When `fee_overrides` is `Some`, the provided [`FeeOverride`] values
    /// are used as a floor — the transaction will use
    /// `max(network_fee, override)` for each component. This ensures that
    /// replacement transactions always meet the bumped fee thresholds
    /// required by geth's tx-replacement rules, even if network fees have
    /// dropped since the bump was calculated.
    ///
    /// # Cancellation safety
    ///
    /// This future is **not** cancellation-safe. If dropped after nonce
    /// acquisition (step 6) but before signing completes (step 7), the
    /// nonce is consumed without producing a transaction, creating a
    /// permanent nonce gap. Callers must not wrap this future in
    /// `tokio::select!`, `tokio::time::timeout`, or similar combinators
    /// that may drop it mid-flight.
    ///
    /// # Errors
    ///
    /// Returns classified RPC errors, [`TxManagerError::FeeLimitExceeded`],
    /// nonce errors, or signing errors.
    pub async fn craft_tx(
        &self,
        candidate: &TxCandidate,
        fee_overrides: Option<FeeOverride>,
    ) -> TxManagerResult<PreparedTx> {
        self.craft_tx_with_caps(candidate, fee_overrides, None, None, None).await
    }

    /// Internal variant of [`craft_tx`](Self::craft_tx) that optionally uses
    /// pre-fetched fee caps instead of querying the provider again.
    ///
    /// When `nonce_override` is `Some(n)`, the provided nonce is used directly
    /// without acquiring one from the [`NonceManager`]. This is used by
    /// [`send_async`] to honour pre-assigned nonces for call-order guarantees.
    async fn craft_tx_with_caps(
        &self,
        candidate: &TxCandidate,
        fee_overrides: Option<FeeOverride>,
        caps: Option<GasPriceCaps>,
        nonce_override: Option<u64>,
        cached_sidecar: Option<Arc<BlobTransactionSidecarEip7594>>,
    ) -> TxManagerResult<PreparedTx> {
        let is_blob = candidate.is_blob();

        // Blob transactions must have a recipient address (no contract creation).
        if is_blob && candidate.to.is_none() {
            return Err(TxManagerError::Unsupported(
                "blob transactions must have a recipient address".to_string(),
            ));
        }

        // Reject blob transactions that exceed the per-transaction blob limit.
        if is_blob && candidate.blobs.len() > crate::MAX_BLOBS_PER_TX {
            return Err(TxManagerError::Unsupported(format!(
                "blob count {} exceeds maximum {} per transaction",
                candidate.blobs.len(),
                crate::MAX_BLOBS_PER_TX,
            )));
        }

        let (tip_cap, fee_cap, blob_fee_cap) = if is_blob {
            // Blob transactions: the policy's fees, with the overrides as a floor,
            // such as the replacement fees the send loop computed. The policy's
            // upper bound limits their `max_priority_fee_per_gas` instead of the
            // fee limit.
            let (base_fee, blob_base_fee) = self.blob_base_fees().await?;
            let fees = self.blob_fee_policy.fees(
                candidate.blobs.len() as u128,
                base_fee,
                blob_base_fee,
                fee_overrides.unwrap_or_default(),
            )?;
            self.metrics.record_tipcap(fees.gas_tip_cap as f64 / WEI_PER_GWEI);
            self.metrics
                .record_blob_fee_cap(fees.blob_fee_cap.unwrap_or_default() as f64 / WEI_PER_GWEI);
            (fees.gas_tip_cap, fees.gas_fee_cap, fees.blob_fee_cap)
        } else {
            // Step 1: Get fee estimates.
            let caps = match caps {
                Some(caps) => caps,
                None => self.suggest_gas_price_caps().await?,
            };

            // Step 2: Apply fee overrides as a floor.
            //
            // During fee bumps the caller passes the bumped (tip, fee_cap)
            // as overrides. We take the max of the fresh network estimate
            // and the override so the replacement transaction is guaranteed
            // to satisfy geth's replacement rules even if network fees have
            // dropped since the bump was calculated.
            let (tip_cap, fee_cap) =
                fee_overrides.as_ref().map_or((caps.gas_tip_cap, caps.gas_fee_cap), |fo| {
                    (caps.gas_tip_cap.max(fo.gas_tip_cap), caps.gas_fee_cap.max(fo.gas_fee_cap))
                });

            // Step 3: Check fee limits.
            //
            // The `suggested` parameter is the raw gas_fee_cap computed from
            // the provider's values before enforcing our configured minimums
            // (`min_tip_cap`, `min_basefee`). This detects when enforced
            // minimums or fee overrides inflate the fee cap beyond
            // `fee_limit_multiplier × raw_provider_fee_cap`.
            self.check_fee_limit(fee_cap, caps.raw_gas_fee_cap)?;
            (tip_cap, fee_cap, None)
        };

        // Step 4: Build TransactionRequest.
        let from =
            <EthereumWallet as NetworkWallet<Ethereum>>::default_signer_address(&self.wallet);
        let mut tx_request = TransactionRequest::default()
            .with_max_fee_per_gas(fee_cap)
            .with_max_priority_fee_per_gas(tip_cap)
            .with_value(candidate.value)
            .with_chain_id(self.chain_id);
        tx_request.input = Some(candidate.tx_data.clone()).into();

        tx_request.set_from(from);

        match candidate.to {
            Some(to) => tx_request.set_to(to),
            None => tx_request = tx_request.into_create(),
        }

        // Step 4b: Attach blob sidecar and blob-specific fields.
        //
        // Reuse a cached sidecar when available (fee bump iterations) to
        // avoid recomputing KZG proofs from the same blob data.
        let built_sidecar = if is_blob {
            let sidecar = match cached_sidecar {
                Some(cached) => cached,
                None => Arc::new(BlobTxBuilder::build_sidecar(&candidate.blobs)?),
            };
            tx_request.sidecar = Some((*sidecar).clone().into());
            tx_request.populate_blob_hashes();
            tx_request.max_fee_per_blob_gas = blob_fee_cap;
            Some(sidecar)
        } else {
            None
        };

        // Step 5: Gas estimation.
        //
        // Always call estimate_gas to enforce intrinsic gas checks (e.g.
        // the 21,000 minimum for plain value transfers). When the caller
        // supplies an explicit gas_limit, it is used as a floor via max()
        // so the transaction never under-provisions gas.
        //
        // Strip the blob sidecar for the estimation call — some nodes
        // reject sidecar data in `eth_estimateGas`. Versioned hashes and
        // `max_fee_per_blob_gas` remain so intrinsic-gas accounting is
        // correct.
        let sidecar_stash = tx_request.sidecar.take();
        let estimated = RuntimeTimeout::run(
            &self.runtime,
            self.config.network_timeout,
            self.provider.estimate_gas(tx_request.clone()),
        )
        .await
        .map_err(|_| self.rpc_error("estimate_gas timed out"))?
        .map_err(|e| self.classify_and_record_rpc(&e))?;
        tx_request.sidecar = sidecar_stash;
        let gas_floor = fee_overrides.as_ref().map_or(0, |fo| fo.gas_limit_floor);
        let gas_limit = candidate.gas_limit.max(gas_floor).max(estimated);
        tx_request = tx_request.with_gas_limit(gas_limit);

        // Step 6: Assign nonce.
        //
        // When a nonce was pre-assigned (via `send_async`), use it directly
        // without acquiring the nonce manager lock. Otherwise, acquire a
        // `NonceGuard` so the nonce can be rolled back on sign failure.
        let (nonce, guard) = match nonce_override {
            Some(n) => (n, None),
            None => {
                let g = self.nonce_manager.next_nonce().await?;
                let n = g.nonce();
                (n, Some(g))
            }
        };
        tx_request = tx_request.with_nonce(nonce);
        self.metrics.record_current_nonce(nonce);

        info!(
            nonce,
            gas_limit = %gas_limit,
            tip_cap = %tip_cap,
            fee_cap = %fee_cap,
            blob_fee_cap = ?blob_fee_cap,
            "transaction crafted",
        );

        // Step 7: Sign and encode.
        let sign_result: Result<
            <Ethereum as Network>::TxEnvelope,
            TransactionBuilderError<Ethereum>,
        > = <TransactionRequest as NetworkTransactionBuilder<Ethereum>>::build(
            tx_request,
            &self.wallet,
        )
        .await;

        match sign_result {
            Ok(envelope) => {
                // Consume the nonce (drop guard if present).
                drop(guard);

                // Record max possible fee metric in gwei. Intentionally recorded on every
                // attempt (initial + fee bumps) to track fee escalation across retries.
                let fee_gwei = gas_limit as f64 * (fee_cap as f64 / WEI_PER_GWEI);
                self.metrics.record_tx_max_fee(fee_gwei);

                Ok(PreparedTx {
                    raw_tx: Bytes::from(Encodable2718::encoded_2718(&envelope)),
                    gas_tip_cap: tip_cap,
                    gas_fee_cap: fee_cap,
                    blob_fee_cap,
                    gas_limit,
                    nonce,
                    sidecar: built_sidecar,
                })
            }
            Err(e) => {
                // Roll back nonce on sign failure (only when guard-managed).
                if let Some(g) = guard {
                    g.rollback();
                }
                Err(TxManagerError::Sign(e.to_string()))
            }
        }
    }

    /// Main send event loop that drives a transaction from publication to
    /// onchain confirmation.
    ///
    /// Orchestrates the send lifecycle:
    /// 1. Prepare and sign the initial transaction via [`prepare`](Self::prepare).
    /// 2. Publish the raw transaction via [`publish_tx`](Self::publish_tx).
    /// 3. Spawn a background [`wait_for_tx`](Self::wait_for_tx) task to poll
    ///    for the receipt.
    /// 4. Enter a `tokio::select!` loop that monitors the resubmission timer
    ///    (for fee bumping), the receipt channel, and critical errors.
    ///
    /// # Receipt status
    ///
    /// The returned receipt is **not** inspected for EVM execution status.
    /// A reverted transaction that reaches the required confirmation depth
    /// is returned as `Ok(receipt)`. Callers must check
    /// `receipt.inner.status()` to distinguish successful execution from
    /// reverts.
    ///
    /// # Nonce reset on error
    ///
    /// When `send_tx` exits with an error, the nonce manager may be reset
    /// to prevent stale-counter gaps on the next call:
    ///
    /// * **Pre-publish errors** before signing reset the cache. After a nonce
    ///   is signed, it is returned to the reuse pool instead so concurrent
    ///   `send()` calls cannot rewind past an in-flight higher nonce.
    ///   `send_async` recycles its pre-reserved nonce so concurrent
    ///   reservations remain valid.
    /// * **`NonceTooHigh`** is retried with the same signed transaction before
    ///   reaching cleanup.
    /// * **`SendTimeout`**: the nonce is always reset — even after a
    ///   successful publish — because the timeout may have cancelled
    ///   `prepare()` mid-flight during a fee bump, leaking a nonce from
    ///   the nonce manager's internal counter.
    /// * **Post-publish errors** (other than timeout): the nonce is *not*
    ///   reset, because a transaction may still be pending and re-fetching
    ///   the `latest` nonce could conflict with it.
    ///
    /// # Errors
    ///
    /// Returns the confirmed [`TransactionReceipt`] on success, or a
    /// [`TxManagerError`] on critical errors, timeout, or manager shutdown.
    async fn send_tx(&self, candidate: TxCandidate, nonce_override: Option<u64>) -> SendResponse {
        if self.is_closed() {
            return Err(TxManagerError::ChannelClosed);
        }

        let send_state = Arc::new(SendState::new(self.config.safe_abort_nonce_too_low_count)?);

        // Set mempool deadline if configured.
        if !self.config.tx_not_in_mempool_timeout.is_zero() {
            send_state.set_runtime_mempool_deadline(
                self.runtime.now() + self.config.tx_not_in_mempool_timeout,
            );
        }

        let start = self.runtime.now();
        // Inclusion is measured against an L1 block timestamp, which is wall-clock time.
        let start_unix_ms = self.runtime.wall_clock_unix_millis();

        // Wrap the event loop in a send timeout if configured.
        let result = if self.config.tx_send_timeout.is_zero() {
            self.send_event_loop(&candidate, &send_state, nonce_override).await
        } else {
            RuntimeTimeout::run(
                &self.runtime,
                self.config.tx_send_timeout,
                self.send_event_loop(&candidate, &send_state, nonce_override),
            )
            .await
            .unwrap_or_else(|_| {
                warn!(
                    timeout = ?self.config.tx_send_timeout,
                    "send timed out",
                );
                Err(TxManagerError::SendTimeout)
            })
        };

        let latency_ms =
            u64::try_from(self.runtime.now().saturating_sub(start).as_millis()).unwrap_or(u64::MAX);

        if let Ok(confirmed) = &result {
            self.metrics.record_send_latency(latency_ms, SendOutcome::Confirmed);
            // Recorded with the send latency, so both series cover the same sends.
            let included_unix_ms = confirmed.block_timestamp.saturating_mul(1_000);
            self.metrics.record_inclusion_latency(included_unix_ms.saturating_sub(start_unix_ms));
            self.metrics.record_tx_confirmed();
        } else {
            self.metrics.record_send_latency(latency_ms, SendOutcome::Failed);
            self.metrics.record_tx_failed();
        }

        if Self::should_reset_nonce_on_send_error(&result, &send_state, nonce_override) {
            // Two policies (see should_reset_nonce_on_send_error):
            // • SendTimeout — always reset. The timeout may have cancelled
            //   prepare() after it reserved a nonce but before the tx reached
            //   the mempool, leaking the nonce manager's internal counter.
            // • Other errors — reset only when no nonce was ever signed, so
            //   the cache cannot rewind past a nonce another task is using.
            self.nonce_manager.reset().await;
        }

        // Return nonces that never reached the mempool so they can be
        // reissued. `send_async` reserves its nonce up front, `send` records
        // the one it signed with.
        if let Some(n) = nonce_override.or_else(|| send_state.assigned_nonce())
            && Self::should_return_reserved_nonce(&result, &send_state)
        {
            self.nonce_manager.return_reserved_nonce(n).await;
        }

        result.map(|confirmed| confirmed.receipt)
    }

    fn should_reset_nonce_on_send_error<T>(
        result: &TxManagerResult<T>,
        send_state: &SendState,
        nonce_override: Option<u64>,
    ) -> bool {
        match result {
            Ok(_) => false,
            // When a nonce_override was used, the nonce was irrevocably
            // consumed at reserve_nonce() time. Resetting the nonce manager
            // would corrupt concurrent send_async reservations.
            _ if nonce_override.is_some() => false,
            // Always reset on timeout — the timeout may have cancelled
            // prepare() mid-flight during a fee bump, leaking a nonce
            // from the nonce manager's internal counter.
            Err(TxManagerError::SendTimeout) => true,
            // Reset only when this send never signed a nonce. A signed nonce
            // goes back to the reuse pool instead, so a concurrent `send()`
            // cannot rewind the cache past an in-flight higher nonce while a
            // `NonceTooHigh` retry is still sleeping.
            Err(_) => send_state.assigned_nonce().is_none(),
        }
    }

    /// Returns `true` when an unpublished nonce should be returned to the
    /// nonce manager's reuse pool.
    ///
    /// A nonce is eligible for return when BOTH of these hold:
    /// 1. The send failed (`result.is_err()`).
    /// 2. No transaction was ever published — if a tx was published, or its
    ///    publish got no answer, the nonce may be in the mempool and must not
    ///    be reused.
    fn should_return_reserved_nonce<T>(
        result: &TxManagerResult<T>,
        send_state: &SendState,
    ) -> bool {
        match result {
            // A too-low nonce was already consumed and must not be reused.
            Ok(_) | Err(TxManagerError::NonceTooLow) => false,
            Err(_) => !send_state.has_published(),
        }
    }

    /// Drives a signed transaction from publication through mempool to onchain
    /// confirmation.
    ///
    /// `nonce_override` is forwarded to the **initial**
    /// [`prepare_with_initial_caps`](Self::prepare_with_initial_caps) call.
    /// Fee bump paths reuse the original nonce via their own `nonce_override`
    /// to avoid corrupting concurrent `send_async` reservations.
    ///
    /// When a blob transaction lands, [`BlobFeePolicy`] and the metrics record the
    /// `max_priority_fee_per_gas` of the version that landed.
    async fn send_event_loop(
        &self,
        candidate: &TxCandidate,
        send_state: &Arc<SendState>,
        nonce_override: Option<u64>,
    ) -> TxManagerResult<ConfirmedTx> {
        // Initial transaction preparation. prepare() is NOT cancellation-safe,
        // so it runs to completion before entering the select loop.
        // The returned PreparedTx carries the actual on-wire fees, eliminating
        // the need for a separate suggest_gas_price_caps() call.
        // A revert is only an error here. During a fee bump it usually means a published
        // version already executed, and `try_fee_bump` reports it as a warning.
        let prepared = self
            .prepare_with_initial_caps(candidate, None, None, nonce_override, None)
            .await
            .inspect_err(|error| {
                if matches!(error, TxManagerError::ExecutionReverted { .. }) {
                    error!(error = %error, "gas estimation reverted");
                }
            })?;
        send_state.record_assigned_nonce(prepared.nonce);
        let (prepared, tx_hash) =
            self.publish_version(candidate, send_state, prepared, None).await?;
        let mut bump = BumpState::new(prepared, tx_hash);

        // Receipt delivery channel — mpsc because fee bumps may spawn
        // new wait tasks with different tx hashes.
        let (receipt_tx, mut receipt_rx) = mpsc::channel::<ConfirmedTx>(1);

        // Spawn background receipt polling for the initial tx hash.
        self.spawn_wait_for_tx(Arc::clone(send_state), bump.tx_hash, receipt_tx.clone());

        // Resubmission timer for fee bumping.
        let mut bump_ticker = self.runtime.interval(self.config.resubmission_timeout);
        // Consume the first immediate tick.
        bump_ticker.next().await;

        let confirmed = loop {
            // Check shutdown and critical errors before blocking.
            if self.is_closed() {
                return Err(TxManagerError::ChannelClosed);
            }
            if let Some(err) = send_state.critical_error_at(self.runtime.now()) {
                error!(error = %err, "critical error, aborting send");
                return Err(err);
            }

            // If a receipt is already waiting, return it immediately
            // instead of performing a wasted fee bump.
            if let Ok(confirmed) = receipt_rx.try_recv() {
                break confirmed;
            }

            // Preparation is not cancellation-safe, so fee bump work must run
            // to completion outside the select block.
            tokio::select! {
                biased;
                result = receipt_rx.recv() => {
                    match result {
                        Some(confirmed) => break confirmed,
                        None => {
                            // All senders dropped — should not happen in normal flow.
                            return Err(TxManagerError::ChannelClosed);
                        }
                    }
                }
                () = self.closed.cancelled() => return Err(TxManagerError::ChannelClosed),
                _ = bump_ticker.next() => {}
            };

            self.try_fee_bump(candidate, send_state, &receipt_tx, &mut bump).await;
            // Even a failed publication must wait a full interval before another bump.
            bump_ticker = self.runtime.interval(self.config.resubmission_timeout);
            bump_ticker.next().await;
        };

        info!(
            tx_hash = %confirmed.receipt.transaction_hash,
            block = ?confirmed.receipt.block_number,
            status = %confirmed.receipt.inner.status(),
            "transaction receipt received",
        );
        if candidate.is_blob() {
            let blob_count = candidate.blobs.len() as u128;
            let max_priority_fee_per_gas = bump
                .published
                .iter()
                .find(|(tx_hash, _)| *tx_hash == confirmed.receipt.transaction_hash)
                .map_or(bump.prepared.gas_tip_cap, |(_, fee)| *fee);
            self.blob_fee_policy.record_landed(max_priority_fee_per_gas, blob_count);
            self.metrics.record_blob_tx_landed(
                max_priority_fee_per_gas as f64 / blob_count as f64 / WEI_PER_GWEI,
                bump.published.len() as u64 - 1,
            );
        }
        Ok(confirmed)
    }

    /// Performs a fee bump attempt and applies the result to the tracked state.
    ///
    /// A failed bump is logged and the send keeps waiting, because a version published
    /// earlier may still be mined. Aborting is left to [`SendState::critical_error_at`].
    async fn try_fee_bump(
        &self,
        candidate: &TxCandidate,
        send_state: &Arc<SendState>,
        receipt_tx: &mpsc::Sender<ConfirmedTx>,
        state: &mut BumpState,
    ) {
        // If a receipt-polling task has already detected a published version
        // on-chain, skip the fee bump entirely. This avoids a race where
        // `estimate_gas` on the replacement tx reverts because the
        // calldata has already executed (e.g. `GameAlreadyExists`),
        // producing a noisy warning even though the proposal will succeed.
        if send_state.is_waiting_for_confirmation() {
            info!("skipping fee bump, a published tx is already mined and awaits confirmation");
            return;
        }

        match self.handle_fee_bump(candidate, send_state, receipt_tx, state).await {
            Ok((prepared, tx_hash)) => state.replace(prepared, tx_hash),
            Err(error) => warn!(error = %error, "fee bump failed, will retry next tick"),
        }
    }

    /// Handles a single fee bump iteration.
    ///
    /// Computes the replacement fees, rebuilds the transaction reusing the same
    /// nonce via `nonce_override`, re-publishes it, and spawns a new receipt
    /// polling task.
    ///
    /// A regular transaction is bumped with
    /// [`increase_gas_price`](Self::increase_gas_price), whose fees are passed
    /// as fee overrides to
    /// [`prepare_with_initial_caps`](Self::prepare_with_initial_caps). There,
    /// each override is used as a floor via `max(network_fee, override)`,
    /// guaranteeing the replacement transaction meets geth's replacement
    /// thresholds even if network fees have dropped since the bump was
    /// calculated. A blob transaction is replaced by
    /// [`replace_blob_tx`](Self::replace_blob_tx).
    ///
    /// Returns the published replacement and its hash. Its fee values are the
    /// *actual* on-wire fees (which are guaranteed >= the bumped values), so
    /// that subsequent bump iterations use an accurate baseline.
    async fn handle_fee_bump(
        &self,
        candidate: &TxCandidate,
        send_state: &Arc<SendState>,
        receipt_tx: &mpsc::Sender<ConfirmedTx>,
        old: &BumpState,
    ) -> TxManagerResult<(PreparedTx, B256)> {
        let old_tx = &old.prepared;
        let prepared = if candidate.is_blob() {
            self.replace_blob_tx(candidate, old_tx).await?
        } else {
            let bumped = self.increase_gas_price(old_tx.gas_tip_cap, old_tx.gas_fee_cap).await?;

            // Rebuild transaction with bumped fees as overrides and the fresh
            // caps to avoid a redundant provider round-trip.
            // Reuse the exact nonce from the transaction being replaced via
            // nonce_override, rather than resetting the nonce manager.
            // Resetting would corrupt nonces pre-reserved by concurrent
            // send_async() tasks.
            self.prepare_with_initial_caps(
                candidate,
                Some(bumped.to_fee_override(old_tx.gas_limit)),
                Some(bumped.caps),
                Some(old_tx.nonce),
                None,
            )
            .await?
        };

        let (prepared, new_hash) =
            self.publish_version(candidate, send_state, prepared, Some(old.tx_hash)).await?;

        // Record the bump and log only after the transaction has been
        // successfully published to avoid inflating the count on failure.
        let bump_count = send_state.record_fee_bump();
        self.metrics.record_gas_bump();
        info!(
            bump_count = %bump_count,
            old_tip = %old_tx.gas_tip_cap,
            new_tip = %prepared.gas_tip_cap,
            old_fee_cap = %old_tx.gas_fee_cap,
            new_fee_cap = %prepared.gas_fee_cap,
            old_blob_fee_cap = ?old_tx.blob_fee_cap,
            new_blob_fee_cap = ?prepared.blob_fee_cap,
            old_gas_limit = %old_tx.gas_limit,
            new_gas_limit = %prepared.gas_limit,
            "fee bump applied",
        );

        // Spawn a new receipt polling task for the bumped tx.
        self.spawn_wait_for_tx(Arc::clone(send_state), new_hash, receipt_tx.clone());

        Ok((prepared, new_hash))
    }

    /// Publishes `prepared`, a version of `candidate`, and returns the version that
    /// was published with its hash.
    ///
    /// The first version of a send (`last_tx_hash` is `None`) is published with
    /// [`publish_tx_with_nonce_retry`](Self::publish_tx_with_nonce_retry), a
    /// replacement with [`publish_tx`](Self::publish_tx).
    ///
    /// A version whose publish got no answer ([`TxManagerError::NoAnswer`]) may have
    /// reached the node, so it is returned as published: the send polls its receipt,
    /// replaces it in the next fee bump, and keeps its nonce.
    ///
    /// A blob transaction that the node refuses for its fees is replaced right
    /// away by [`replace_blob_tx`](Self::replace_blob_tx), from the refused fees,
    /// until a version is accepted or no replacement exists within the upper bound
    /// of [`BlobFeePolicy`]. A blob version the node already knows is published
    /// under its own hash. After an RPC error or no answer, such as a network
    /// error, the same signed blob version is sent again every
    /// `publish_retry_delay`, up to `publish_max_retries` times, without raising any
    /// fee. Once a publish of a blob version got no answer, that version may be in a
    /// mempool, so it is not replaced here any more: every later error, a fee refusal
    /// included, returns it as published, and the send polls its receipt. Every other
    /// error is returned. Before each new attempt, a closed manager or a critical error
    /// of `send_state`, such as an expired mempool deadline, ends the publication. Each
    /// published blob replacement raises the fees the next blob transactions start
    /// from.
    async fn publish_version(
        &self,
        candidate: &TxCandidate,
        send_state: &SendState,
        mut prepared: PreparedTx,
        last_tx_hash: Option<B256>,
    ) -> TxManagerResult<(PreparedTx, B256)> {
        let mut is_replacement = last_tx_hash.is_some();
        let mut remaining_retries = self.config.publish_max_retries;
        let mut is_retry = false;
        // Whether a publish of this blob version got no answer. It may then be in a mempool,
        // so it is no longer replaced here.
        let mut unanswered = false;
        loop {
            if is_retry {
                if self.is_closed() {
                    return Err(TxManagerError::ChannelClosed);
                }
                if let Some(err) = send_state.critical_error_at(self.runtime.now()) {
                    return Err(err);
                }
            }
            is_retry = true;
            let result = match last_tx_hash {
                Some(_) => self.publish_tx(send_state, &prepared.raw_tx, last_tx_hash).await,
                None => self.publish_tx_with_nonce_retry(send_state, &prepared.raw_tx).await,
            };
            // The hash of this version, also when the node already knows it: `publish_tx`
            // then returns the hash of the previous version.
            let tx_hash = *prepared
                .to_envelope()
                .map_err(|error| TxManagerError::Sign(error.to_string()))?
                .tx_hash();
            if !candidate.is_blob() {
                return match result {
                    Err(TxManagerError::NoAnswer) => Ok((prepared, tx_hash)),
                    result => result.map(|tx_hash| (prepared, tx_hash)),
                };
            }
            match result {
                Ok(_) => {}
                // `publish_tx` already accepts it once a version was published. A first
                // version the node knows is the same signed transaction, in its mempool.
                Err(TxManagerError::AlreadyKnown) => {
                    send_state.record_publish();
                    info!(tx_hash = %tx_hash, "blob transaction already known in mempool");
                }
                Err(error) if !unanswered && error.is_fee_rejection() => {
                    info!(error = %error, "blob transaction refused for its fees, replacing it");
                    prepared = self.replace_blob_tx(candidate, &prepared).await?;
                    is_replacement = true;
                    continue;
                }
                Err(error @ (TxManagerError::Rpc(_) | TxManagerError::NoAnswer))
                    if remaining_retries > 0 =>
                {
                    unanswered |= error == TxManagerError::NoAnswer;
                    remaining_retries -= 1;
                    warn!(
                        error = %error,
                        delay = ?self.config.publish_retry_delay,
                        "resending the same blob transaction",
                    );
                    self.runtime.sleep(self.config.publish_retry_delay).await;
                    continue;
                }
                Err(error) if unanswered || error == TxManagerError::NoAnswer => {
                    warn!(
                        error = %error,
                        tx_hash = %tx_hash,
                        "following a blob transaction whose publication got no answer",
                    );
                }
                Err(error) => return Err(error),
            }
            if is_replacement {
                self.blob_fee_policy
                    .record_replacement(prepared.gas_tip_cap, candidate.blobs.len() as u128);
            }
            return Ok((prepared, tx_hash));
        }
    }

    /// Signs the replacement of `old`, a version of the blob transaction
    /// `candidate`, with the fees of [`BlobFeePolicy::replacement`] as a floor over
    /// the policy's fees at the current L1 prices, the same nonce and a gas limit no
    /// lower than `old`'s.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::FeeLimitExceeded`] when no replacement exists within
    /// the upper bound of the policy, or the error of the preparation.
    async fn replace_blob_tx(
        &self,
        candidate: &TxCandidate,
        old: &PreparedTx,
    ) -> TxManagerResult<PreparedTx> {
        let fees = self.blob_fee_policy.replacement(old, candidate.blobs.len() as u128)?;
        self.prepare_with_initial_caps(
            candidate,
            Some(fees.with_gas_limit_floor(old.gas_limit)),
            None,
            Some(old.nonce),
            old.sidecar.clone(),
        )
        .await
    }

    /// Publishes the initial transaction, retrying a transient nonce gap.
    ///
    /// Concurrent sends reserve nonces in order but may reach different RPC
    /// backends out of order. Retrying the same signed transaction lets the
    /// lower nonce propagate without serializing receipt confirmation or fee
    /// bumping.
    async fn publish_tx_with_nonce_retry(
        &self,
        send_state: &SendState,
        raw_tx: &Bytes,
    ) -> TxManagerResult<B256> {
        let mut remaining_retries = self.config.publish_max_retries;
        let mut is_retry = false;
        loop {
            if self.is_closed() {
                return Err(TxManagerError::ChannelClosed);
            }
            if is_retry && let Some(err) = send_state.critical_error_at(self.runtime.now()) {
                return Err(err);
            }

            match self.publish_tx(send_state, raw_tx, None).await {
                Ok(tx_hash) => return Ok(tx_hash),
                Err(TxManagerError::NonceTooHigh) if remaining_retries > 0 => {
                    remaining_retries -= 1;
                    if let Some(err) = send_state.critical_error_at(self.runtime.now()) {
                        return Err(err);
                    }
                    warn!(
                        error = %TxManagerError::NonceTooHigh,
                        delay = ?self.config.publish_retry_delay,
                        "retrying initial transaction publication"
                    );
                    self.runtime.sleep(self.config.publish_retry_delay).await;
                    is_retry = true;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Broadcasts a raw transaction to the network.
    ///
    /// On success, records a publish on the [`SendState`] and returns the locally
    /// computed transaction hash. On [`TxManagerError::AlreadyKnown`] after a prior
    /// publish with a known hash, treats it as success — records another publish and
    /// returns `Ok(last_tx_hash)` so callers do not need to special-case this variant.
    /// Other JSON-RPC error answers are forwarded to [`SendState::process_send_error`]
    /// for state tracking.
    ///
    /// A publish that gets no JSON-RPC answer (see [`TxManagerError::NoAnswer`]) may
    /// still have reached the node, so it is recorded as a publish too, which keeps the
    /// nonce of the send from being reused.
    ///
    /// The `last_tx_hash` parameter is the hash from the most recent
    /// publish. When the network returns `AlreadyKnown` on
    /// resubmission, this hash is returned as the successful result.
    ///
    /// The returned hash never comes from the RPC. It is the decoded envelope's
    /// [`TxEnvelope::tx_hash`], which excludes any EIP-4844 blob sidecar. If `raw_tx`
    /// does not decode as a known envelope, it falls back to `keccak256(raw_tx)`, the
    /// EIP-2718 hash of any typed transaction whose network encoding equals its
    /// consensus encoding; a well-formed RPC hash is not preferred, because the RPC
    /// reply is advisory. A different or undecodable hash in a valid JSON-RPC success
    /// response only logs a warning; only a transport failure, a timeout, an invalid
    /// response envelope, or a JSON-RPC error response is a publication error.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::NoAnswer`] when the publish timed out or failed in
    /// transport after the request may have reached the node (see
    /// [`RpcErrorClassifier::is_unanswered`]), and the classified error on any other
    /// publication failure.
    pub async fn publish_tx(
        &self,
        send_state: &SendState,
        raw_tx: &Bytes,
        last_tx_hash: Option<B256>,
    ) -> TxManagerResult<B256> {
        let local_tx_hash = match TxEnvelope::decode_2718_exact(raw_tx) {
            Ok(envelope) => *envelope.tx_hash(),
            Err(error) => {
                // Unknown transaction types must not turn a successful broadcast into an error.
                warn!(%error, "could not decode transaction, hashing raw bytes as a fallback");
                keccak256(raw_tx)
            }
        };
        // Decode the result separately so invalid response envelopes remain publication errors.
        let result = RuntimeTimeout::run(
            &self.runtime,
            self.config.network_timeout,
            self.provider
                .client()
                .request::<_, serde_json::Value>("eth_sendRawTransaction", (raw_tx,)),
        )
        .await;

        match result {
            Ok(Ok(response)) => {
                match serde_json::from_value::<B256>(response) {
                    Ok(rpc_tx_hash) if rpc_tx_hash != local_tx_hash => {
                        warn!(
                            local_tx_hash = %local_tx_hash,
                            rpc_tx_hash = %rpc_tx_hash,
                            "RPC returned a different transaction hash, using locally computed hash"
                        );
                    }
                    Err(_) => {
                        warn!(
                            local_tx_hash = %local_tx_hash,
                            "RPC returned an undecodable transaction hash, treating publication as successful"
                        );
                    }
                    Ok(_) => {}
                }
                send_state.record_publish();
                info!(tx_hash = %local_tx_hash, "transaction published");
                return Ok(local_tx_hash);
            }
            Ok(Err(e)) if RpcErrorClassifier::is_unanswered(&e) => {
                self.metrics.record_rpc_error();
            }
            Ok(Err(e)) => {
                let classified = self.classify_and_record_rpc(&e);
                // AlreadyKnown on resubmission is a success — the tx is in
                // the mempool from a prior publish. Return the previous hash.
                // A first version resent after a publish that got no answer has
                // no previous hash: its caller handles the error.
                if classified.is_already_known()
                    && send_state.has_published()
                    && let Some(hash) = last_tx_hash
                {
                    send_state.record_publish();
                    info!(tx_hash = %hash, "transaction already known in mempool, treating as success");
                    return Ok(hash);
                }

                send_state.process_send_error(&classified);
                if classified.is_rpc_error() {
                    self.metrics.record_publish_error();
                }

                if classified.is_retryable() {
                    warn!(error = %classified, "publish failed, will retry");
                } else if matches!(classified, TxManagerError::ExecutionReverted { .. }) {
                    error!(error = %classified, "transaction execution reverted");
                } else if matches!(classified, TxManagerError::NonceTooLow)
                    && send_state.has_published()
                {
                    warn!(error = %classified, "nonce too low, a published version may be mined");
                } else {
                    error!(error = %classified, "publish failed with non-retryable error");
                }

                return Err(classified);
            }
            Err(_) => self.metrics.record_rpc_error(),
        }

        // Without an answer, the node may have received the transaction. Its nonce
        // must stay with this send, which may still see it mined.
        send_state.record_publish();
        self.metrics.record_publish_error();
        warn!(tx_hash = %local_tx_hash, "publish got no answer, the transaction may have reached the node");
        Err(TxManagerError::NoAnswer)
    }

    /// Spawns a background task that polls for a transaction receipt and
    /// sends it through the provided channel once confirmed.
    ///
    /// The task delegates to [`wait_mined`](Self::wait_mined), which polls
    /// internally until the transaction is confirmed or the manager shuts down.
    ///
    /// Returns the [`JoinHandle`](tokio::task::JoinHandle) of the spawned
    /// task so callers can await its completion if needed.
    ///
    /// # Task accumulation
    ///
    /// Each fee bump spawns a new polling task for the replacement tx hash
    /// without cancelling the previous one. This is intentional: any
    /// previously-published variant may still confirm (the original or an
    /// earlier bump), and we want to detect whichever is mined first.
    /// Accumulated tasks are bounded by three mechanisms:
    /// - The `closed` flag causes all pollers to exit on shutdown.
    /// - Each poller exits once it delivers a receipt through the mpsc
    ///   channel, or as soon as the receiver is dropped because the send
    ///   loop returned or was cancelled.
    /// - Polling frequency is governed by `receipt_query_interval`, so
    ///   RPC load is proportional to `bump_count × 1/interval`.
    pub fn wait_for_tx(
        send_state: Arc<SendState>,
        provider: P,
        tx_hash: B256,
        config: TxManagerConfig,
        receipt_tx: mpsc::Sender<TransactionReceipt>,
        closed: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            debug!(tx_hash = %tx_hash, "starting receipt polling");

            // Stop polling once the receiver is dropped. Nobody can consume the receipt then.
            let polling = Self::wait_mined(&send_state, &provider, tx_hash, &config, &closed);
            let receipt = tokio::select! {
                biased;
                () = receipt_tx.closed() => None,
                receipt = polling => receipt,
            };
            if let Some(receipt) = receipt {
                // Best-effort send — if the receiver is dropped, the
                // send loop has already exited (e.g., another tx confirmed).
                let _ = receipt_tx.send(receipt).await;
            }

            debug!(tx_hash = %tx_hash, "receipt polling ended");
        })
    }

    /// Spawns a task on the configured runtime that polls the receipt of one published
    /// version of the transaction.
    ///
    /// The task runs until the version is confirmed, the send ends or the manager shuts
    /// down.
    fn spawn_wait_for_tx(
        &self,
        send_state: Arc<SendState>,
        tx_hash: B256,
        receipt_tx: mpsc::Sender<ConfirmedTx>,
    ) {
        let manager = self.clone();
        self.runtime.spawn(async move {
            debug!(tx_hash = %tx_hash, "starting receipt polling");

            // Stop polling once the receiver is dropped. Nobody can consume the receipt then.
            let confirmed = tokio::select! {
                biased;
                () = receipt_tx.closed() => None,
                confirmed = Self::wait_mined_using_runtime(&manager.runtime, &send_state, &manager.provider, tx_hash, &manager.config, &manager.closed) => confirmed,
            };
            if let Some(confirmed) = confirmed {
                let _ = receipt_tx.send(confirmed).await;
            }

            debug!(tx_hash = %tx_hash, "receipt polling ended");
        });
    }

    /// Polls for a transaction receipt at `receipt_query_interval` until
    /// the transaction is mined and confirmed to the required depth.
    ///
    /// Returns `Some(receipt)` when the transaction reaches
    /// `num_confirmations` depth, or `None` if the manager is closed. Callers
    /// that need a bound wrap it in a timeout.
    pub async fn wait_mined(
        send_state: &SendState,
        provider: &P,
        tx_hash: B256,
        config: &TxManagerConfig,
        closed: &CancellationToken,
    ) -> Option<TransactionReceipt> {
        Self::wait_mined_using_runtime(
            &TokioRuntime::new(),
            send_state,
            provider,
            tx_hash,
            config,
            closed,
        )
        .await
        .map(|confirmed| confirmed.receipt)
    }

    /// Runtime-backed implementation of [`wait_mined`](Self::wait_mined).
    async fn wait_mined_using_runtime<Rt>(
        runtime: &Rt,
        send_state: &SendState,
        provider: &P,
        tx_hash: B256,
        config: &TxManagerConfig,
        closed: &CancellationToken,
    ) -> Option<ConfirmedTx>
    where
        Rt: Runtime,
    {
        let mut poll_immediately = true;

        loop {
            if poll_immediately {
                poll_immediately = false;
            } else {
                // Keep the previous MissedTickBehavior::Delay semantics:
                // slow receipt queries do not accumulate immediate catch-up ticks.
                runtime.sleep(config.receipt_query_interval).await;
            }

            match Self::query_receipt_using_runtime(
                runtime,
                send_state,
                provider,
                tx_hash,
                config.num_confirmations,
                config.network_timeout,
            )
            .await
            {
                Ok(Some(confirmed)) => return Some(confirmed),
                Ok(None) => {
                    // Not yet confirmed — continue polling.
                }
                Err(e) => {
                    warn!(tx_hash = %tx_hash, error = %e, "receipt query failed");
                }
            }

            // Check shutdown state each iteration to support cancellation.
            if closed.is_cancelled() {
                debug!(tx_hash = %tx_hash, "manager closed, stopping receipt polling");
                return None;
            }
        }
    }

    /// Performs a single receipt check for the given transaction hash.
    ///
    /// - If no receipt is found, calls [`SendState::tx_not_mined`] (reorg
    ///   detection) and returns `Ok(None)`.
    /// - If a receipt is found, calls [`SendState::tx_mined`] and checks
    ///   confirmation depth via the formula:
    ///   `tx_block + num_confirmations <= tip_height + 1`.
    /// - Returns `Ok(Some(receipt))` when sufficiently confirmed, or
    ///   `Ok(None)` when the receipt exists but needs more confirmations.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::Rpc`] if provider calls fail.
    pub async fn query_receipt(
        send_state: &SendState,
        provider: &P,
        tx_hash: B256,
        num_confirmations: u64,
        network_timeout: Duration,
    ) -> TxManagerResult<Option<TransactionReceipt>> {
        Self::query_receipt_using_runtime(
            &TokioRuntime::new(),
            send_state,
            provider,
            tx_hash,
            num_confirmations,
            network_timeout,
        )
        .await
        .map(|confirmed| confirmed.map(|confirmed| confirmed.receipt))
    }

    /// Runtime-backed implementation of [`query_receipt`](Self::query_receipt).
    async fn query_receipt_using_runtime<Rt>(
        runtime: &Rt,
        send_state: &SendState,
        provider: &P,
        tx_hash: B256,
        num_confirmations: u64,
        network_timeout: Duration,
    ) -> TxManagerResult<Option<ConfirmedTx>>
    where
        Rt: Runtime,
    {
        // Fetch tip and receipt in parallel. The canonical block hash check
        // below ensures the receipt is on the canonical chain, so call order
        // no longer matters for reorg safety.
        let (tip_result, receipt_result) = tokio::join!(
            RuntimeTimeout::run(runtime, network_timeout, provider.get_block_number()),
            RuntimeTimeout::run(
                runtime,
                network_timeout,
                provider.get_transaction_receipt(tx_hash)
            ),
        );

        let tip_height = tip_result
            .map_err(|_| TxManagerError::Rpc("get_block_number timed out".into()))?
            .map_err(|e| RpcErrorClassifier::classify_rpc_error(&e))?;

        let receipt_opt = receipt_result
            .map_err(|_| TxManagerError::Rpc("get_transaction_receipt timed out".into()))?
            .map_err(|e| RpcErrorClassifier::classify_rpc_error(&e))?;

        let receipt = match receipt_opt {
            Some(r) => r,
            None => {
                send_state.tx_not_mined(tx_hash);
                debug!(tx_hash = %tx_hash, "receipt not found, possible reorg");
                return Ok(None);
            }
        };

        let tx_block = match receipt.block_number {
            Some(block) => block,
            None => {
                // Receipt without a block number (e.g., pending) — treat as not yet confirmed.
                send_state.tx_not_mined(tx_hash);
                return Ok(None);
            }
        };

        // Verify the receipt is on the canonical chain by comparing its block
        // hash against the canonical block at the same height.
        let receipt_block_hash = match receipt.block_hash {
            Some(hash) => hash,
            None => {
                send_state.tx_not_mined(tx_hash);
                debug!(tx_hash = %tx_hash, "receipt has no block hash, treating as not mined");
                return Ok(None);
            }
        };

        let canonical_block = RuntimeTimeout::run(
            runtime,
            network_timeout,
            provider.get_block_by_number(BlockNumberOrTag::Number(tx_block)),
        )
        .await
        .map_err(|_| TxManagerError::Rpc("get_block_by_number timed out".into()))?
        .map_err(|e| RpcErrorClassifier::classify_rpc_error(&e))?;

        let Some(block) = canonical_block.filter(|block| block.header.hash == receipt_block_hash)
        else {
            send_state.tx_not_mined(tx_hash);
            debug!(
                tx_hash = %tx_hash,
                tx_block = %tx_block,
                receipt_block_hash = %receipt_block_hash,
                "receipt block hash does not match canonical chain",
            );
            return Ok(None);
        };

        // Receipt is on the canonical chain — record as mined.
        send_state.tx_mined(tx_hash);

        // Check confirmation depth: tx_block + num_confirmations <= tip + 1
        if tx_block.saturating_add(num_confirmations) <= tip_height.saturating_add(1) {
            info!(
                tx_hash = %tx_hash,
                tx_block = %tx_block,
                tip_height = %tip_height,
                num_confirmations = %num_confirmations,
                "transaction confirmed to required depth",
            );
            Ok(Some(ConfirmedTx { receipt, block_timestamp: block.header.timestamp }))
        } else {
            debug!(
                tx_hash = %tx_hash,
                tx_block = %tx_block,
                tip_height = %tip_height,
                num_confirmations = %num_confirmations,
                "waiting for more confirmations",
            );
            Ok(None)
        }
    }
}

impl<P, R> TxManager for SimpleTxManager<P, R>
where
    P: Provider + Clone + Debug + Send + Sync + 'static,
    R: Runtime + Debug,
{
    async fn send(&self, candidate: TxCandidate) -> SendResponse {
        self.send_tx(candidate, None).await
    }

    async fn send_async(&self, candidate: TxCandidate) -> SendHandle {
        let (tx, rx) = oneshot::channel();

        // Reserve nonce BEFORE spawning to guarantee call-order assignment.
        // Two sequential `send_async()` calls will receive monotonically
        // increasing nonces regardless of tokio task scheduling order.
        let nonce = match self.nonce_manager.reserve_nonce().await {
            Ok(n) => Some(n),
            Err(e) => {
                let _ = tx.send(Err(e));
                return SendHandle::new(rx);
            }
        };

        // Clone the manager for the spawned task. The `closed` flag is
        // Arc-wrapped, so the spawned task shares the same shutdown signal
        // as the original manager — calling `close()` on the original will
        // be observed by the background task.
        let manager = self.clone();

        self.runtime.spawn(async move {
            let result = manager.send_tx(candidate, nonce).await;
            let _ = tx.send(result);
        });

        SendHandle::new(rx)
    }

    fn sender_address(&self) -> Address {
        <EthereumWallet as NetworkWallet<Ethereum>>::default_signer_address(&self.wallet)
    }
}

#[cfg(test)]
mod tests {
    //! The receipt-checking provider wraps Alloy's concrete RPC builders, which automock
    //! cannot construct, to reject confirmation under an RPC-supplied hash. The
    //! answer-losing transport fails publishes in transport between mocked answers, which
    //! the mocked client cannot express.

    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        task::Poll,
        time::Duration,
    };

    use alloy_consensus::{
        Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom, SignableTransaction, TxEip1559,
        TxEip4844, TxEip4844WithSidecar,
    };
    use alloy_eips::{Encodable2718, eip4844::Blob};
    use alloy_json_rpc::{RequestPacket, ResponsePacket};
    use alloy_network::EthereumWallet;
    use alloy_node_bindings::Anvil;
    use alloy_primitives::{Address, B256, Bloom, Bytes, TxKind, U256, keccak256};
    use alloy_provider::{Provider, ProviderBuilder, ProviderCall, RootProvider};
    use alloy_rpc_client::RpcClient;
    use alloy_rpc_types_eth::{Block, TransactionReceipt, TransactionRequest};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use alloy_transport::{
        TransportError, TransportErrorKind, TransportFut,
        mock::{Asserter, MockTransport},
    };
    use base_runtime::{
        Clock,
        deterministic::{Config, Context, Runner},
    };
    use mockall::predicate::eq;
    use rstest::rstest;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::mpsc,
    };
    use tokio_util::sync::CancellationToken;
    use tracing::{Event, Level, Subscriber};
    use tracing_subscriber::{
        Layer, Registry,
        layer::{Context as LayerContext, SubscriberExt},
    };

    use super::{PreparedTx, SimpleTxManager, TxEnvelope};
    use crate::{
        BlobFeePolicy, BlobTxBuilder, GasPriceCaps, NonceManager, NoopTxMetrics, SendOutcome,
        SendState, TxCandidate, TxManager, TxManagerConfig, TxManagerError, metrics::MockTxMetrics,
    };

    async fn setup() -> (SimpleTxManager<RootProvider>, alloy_node_bindings::AnvilInstance) {
        let anvil = Anvil::new().spawn();
        let url = anvil.endpoint_url();
        let provider = RootProvider::new_http(url);
        let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
        let wallet = EthereumWallet::from(signer);
        let chain_id = anvil.chain_id();
        let manager = SimpleTxManager::from_wallet(
            provider,
            wallet,
            TxManagerConfig::default(),
            chain_id,
            Arc::new(NoopTxMetrics),
        )
        .await
        .expect("should create manager");
        (manager, anvil)
    }

    #[rstest]
    #[case::latest_by_default(false, 0)]
    #[case::pending_opt_in(true, 1)]
    #[tokio::test]
    async fn nonce_initialization_block_tag(#[case] pending: bool, #[case] expected_nonce: u64) {
        let anvil = Anvil::new().arg("--no-mining").spawn();
        let provider = RootProvider::new_http(anvil.endpoint_url());
        let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
        let _pending = provider
            .send_transaction(
                TransactionRequest::default()
                    .from(signer.address())
                    .to(Address::with_last_byte(0x42))
                    .value(U256::from(1))
                    .gas_limit(21_000),
            )
            .await
            .expect("transaction should enter the mempool");
        let manager = SimpleTxManager::from_wallet(
            provider,
            EthereumWallet::from(signer),
            TxManagerConfig::default(),
            anvil.chain_id(),
            Arc::new(NoopTxMetrics),
        )
        .await
        .unwrap();
        let manager = if pending { manager.with_pending_nonce() } else { manager };

        assert_eq!(manager.nonce_manager().next_nonce().await.unwrap().nonce(), expected_nonce);
    }

    #[derive(Debug, Clone)]
    struct ReceiptHashProvider {
        inner: RootProvider,
        local_tx_hash: B256,
    }

    impl Provider for ReceiptHashProvider {
        fn root(&self) -> &RootProvider {
            &self.inner
        }

        fn get_transaction_receipt(
            &self,
            hash: B256,
        ) -> ProviderCall<(B256,), Option<TransactionReceipt>> {
            assert_eq!(hash, self.local_tx_hash, "receipt polling must use the local hash");
            self.inner.get_transaction_receipt(hash)
        }
    }

    fn signed_eip1559() -> TxEnvelope {
        let tx = TxEip1559 {
            chain_id: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(Address::with_last_byte(0x42)),
            ..Default::default()
        };
        let signature = PrivateKeySigner::random().sign_hash_sync(&tx.signature_hash()).unwrap();
        tx.into_signed(signature).into()
    }

    #[tokio::test]
    async fn publish_tx_confirms_local_hash_when_rpc_hash_differs() {
        let envelope = signed_eip1559();
        let raw_tx = envelope.encoded_2718().into();
        let local_tx_hash = *envelope.tx_hash();
        let rpc_tx_hash = B256::with_last_byte(0x42);
        let (receipt, block) = mined_receipt(local_tx_hash, 10);
        let asserter = Asserter::new();
        asserter.push_success(&1u64);
        asserter.push_success(&rpc_tx_hash);
        asserter.push_success(&10u64);
        asserter.push_success(&Some(&receipt));
        asserter.push_success(&Some(&block));
        let provider = ReceiptHashProvider {
            inner: ProviderBuilder::default().connect_mocked_client(asserter),
            local_tx_hash,
        };
        let manager = SimpleTxManager::from_wallet(
            provider,
            EthereumWallet::from(PrivateKeySigner::random()),
            TxManagerConfig { num_confirmations: 1, ..TxManagerConfig::default() },
            1,
            Arc::new(NoopTxMetrics),
        )
        .await
        .expect("should construct manager");
        let send_state = SendState::new(3).expect("should construct send state");

        let tx_hash = manager
            .publish_tx(&send_state, &raw_tx, None)
            .await
            .expect("an unexpected RPC hash must not fail publication");
        let confirmed = tokio::time::timeout(
            Duration::from_secs(1),
            SimpleTxManager::<ReceiptHashProvider>::wait_mined(
                &send_state,
                manager.provider(),
                tx_hash,
                manager.config(),
                &CancellationToken::new(),
            ),
        )
        .await
        .expect("local transaction should confirm")
        .expect("should return a confirmed receipt");

        assert_eq!(tx_hash, local_tx_hash);
        assert_eq!(confirmed.transaction_hash, local_tx_hash);
        assert_ne!(confirmed.transaction_hash, rpc_tx_hash);
        assert!(send_state.has_published());
    }

    #[tokio::test]
    async fn publish_tx_succeeds_when_rpc_hash_is_undecodable() {
        let envelope = signed_eip1559();
        let raw_tx = envelope.encoded_2718().into();
        let asserter = Asserter::new();
        asserter.push_success(&1u64);
        asserter.push_success(&"0xnot-a-hash");
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        let manager = SimpleTxManager::from_wallet(
            provider,
            EthereumWallet::from(PrivateKeySigner::random()),
            TxManagerConfig::default(),
            1,
            Arc::new(NoopTxMetrics),
        )
        .await
        .expect("should construct manager");
        let send_state = SendState::new(3).expect("should construct send state");

        let tx_hash = manager
            .publish_tx(&send_state, &raw_tx, None)
            .await
            .expect("an undecodable RPC hash must not fail publication");

        assert_eq!(tx_hash, *envelope.tx_hash());
        assert!(send_state.has_published());
    }

    #[rstest]
    #[case::canonical_rpc_hash(false)]
    #[case::mismatched_rpc_hash(true)]
    #[tokio::test]
    async fn publish_tx_tracks_blob_hash_without_sidecar(#[case] mismatched_rpc_hash: bool) {
        let sidecar = BlobTxBuilder::build_sidecar(&[Box::<Blob>::default()]).unwrap();
        let tx = TxEip4844 {
            chain_id: 1,
            gas_limit: 21_000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_blob_gas: 1_000_000_000,
            to: Address::with_last_byte(0x42),
            blob_versioned_hashes: sidecar.versioned_hashes().collect(),
            ..Default::default()
        };
        let signature = PrivateKeySigner::random().sign_hash_sync(&tx.signature_hash()).unwrap();
        let tx: TxEip4844WithSidecar =
            TxEip4844WithSidecar::from_tx_and_sidecar(tx, sidecar.into());
        let envelope: TxEnvelope = tx.into_signed(signature).into();
        let raw_tx = Bytes::from(envelope.encoded_2718());
        let canonical_hash = *envelope.tx_hash();
        assert_ne!(canonical_hash, keccak256(&raw_tx));
        let rpc_hash =
            if mismatched_rpc_hash { B256::with_last_byte(0x42) } else { canonical_hash };
        let asserter = Asserter::new();
        asserter.push_success(&1u64);
        asserter.push_success(&rpc_hash);
        let manager = SimpleTxManager::from_wallet(
            ProviderBuilder::new().connect_mocked_client(asserter),
            EthereumWallet::from(PrivateKeySigner::random()),
            TxManagerConfig::default(),
            1,
            Arc::new(NoopTxMetrics),
        )
        .await
        .unwrap();
        let send_state = SendState::new(3).unwrap();

        let tracked_hash = manager.publish_tx(&send_state, &raw_tx, None).await.unwrap();

        assert_eq!(tracked_hash, canonical_hash);
        assert!(send_state.has_published());
    }

    #[tokio::test]
    async fn publish_tx_does_not_fail_for_an_unknown_transaction_type() {
        let raw_tx = Bytes::from_static(&[0x7f]);
        let expected_hash = keccak256(&raw_tx);
        let asserter = Asserter::new();
        asserter.push_success(&1u64);
        asserter.push_success(&expected_hash);
        let manager = SimpleTxManager::from_wallet(
            ProviderBuilder::new().connect_mocked_client(asserter),
            EthereumWallet::from(PrivateKeySigner::random()),
            TxManagerConfig::default(),
            1,
            Arc::new(NoopTxMetrics),
        )
        .await
        .unwrap();
        let send_state = SendState::new(3).unwrap();

        assert_eq!(manager.publish_tx(&send_state, &raw_tx, None).await.unwrap(), expected_hash);
        assert!(send_state.has_published());
    }

    /// JSON-RPC answer to the chain ID query made when a manager is built.
    const CHAIN_ID_RESPONSE: &str = r#"{"jsonrpc":"2.0","id":0,"result":"0x1"}"#;

    /// Reads one HTTP request with a JSON body from `stream`.
    async fn read_http_request(stream: &mut TcpStream) {
        let mut request = Vec::with_capacity(1024);
        loop {
            assert_ne!(stream.read_buf(&mut request).await.unwrap(), 0);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n")
                && serde_json::from_slice::<serde_json::Value>(&request[end + 4..]).is_ok()
            {
                return;
            }
        }
    }

    /// Writes an HTTP response with `status` and `body` to `stream`.
    async fn write_http_response(stream: &mut TcpStream, status: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    #[rstest]
    #[case::html("<html>rate limited</html>")]
    #[case::empty("")]
    #[case::non_rpc_json(r#"{"message":"rate limited"}"#)]
    #[tokio::test]
    async fn publish_tx_rejects_invalid_response_envelopes(#[case] body: &'static str) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
        let server = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(5), async move {
                for body in [CHAIN_ID_RESPONSE, body] {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    read_http_request(&mut stream).await;
                    write_http_response(&mut stream, "200 OK", body).await;
                }
            })
            .await
            .expect("HTTP server should finish");
        });
        let manager = SimpleTxManager::from_wallet(
            RootProvider::new_http(url),
            EthereumWallet::from(PrivateKeySigner::random()),
            TxManagerConfig { network_timeout: Duration::from_secs(1), ..Default::default() },
            1,
            Arc::new(NoopTxMetrics),
        )
        .await
        .unwrap();
        let send_state = SendState::new(3).unwrap();
        send_state.set_runtime_mempool_deadline(Duration::from_secs(1));

        let result =
            manager.publish_tx(&send_state, &signed_eip1559().encoded_2718().into(), None).await;

        assert_eq!(result, Err(TxManagerError::Rpc("transport request failed".into())));
        assert!(!send_state.has_published());
        assert_eq!(
            send_state.critical_error_at(Duration::from_secs(1)),
            Some(TxManagerError::MempoolDeadlineExpired)
        );
        server.await.unwrap();
    }

    /// How the HTTP server of [`publish_tx_without_a_json_rpc_answer`] treats the publish.
    #[derive(Debug, Clone, Copy)]
    enum PublishFate {
        /// The server reads the request and closes the connection.
        ConnectionClosed,
        /// The server reads the request and never answers.
        NeverAnswered,
        /// The server answers with this HTTP status and no JSON-RPC body.
        Status(&'static str),
        /// The server stops listening, so the connection is refused.
        ConnectionRefused,
    }

    /// A publish without a JSON-RPC answer that may have reached the node, because the
    /// connection closed, the network timeout expired or a gateway lost the answer, is
    /// recorded as a publish, so the send keeps its nonce and the mempool deadline no
    /// longer applies. A refused connection or an HTTP refusal never reached the node: it
    /// is a plain RPC error.
    #[rstest]
    #[case::connection_closed(PublishFate::ConnectionClosed, true)]
    #[case::timed_out(PublishFate::NeverAnswered, true)]
    #[case::gateway_timeout(PublishFate::Status("504 Gateway Timeout"), true)]
    #[case::unavailable(PublishFate::Status("503 Service Unavailable"), false)]
    #[case::connection_refused(PublishFate::ConnectionRefused, false)]
    #[tokio::test]
    async fn publish_tx_without_a_json_rpc_answer(
        #[case] fate: PublishFate,
        #[case] may_have_reached_the_node: bool,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_request(&mut stream).await;
            write_http_response(&mut stream, "200 OK", CHAIN_ID_RESPONSE).await;
            if matches!(fate, PublishFate::ConnectionRefused) {
                return;
            }
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_request(&mut stream).await;
            match fate {
                PublishFate::NeverAnswered => std::future::pending::<()>().await,
                PublishFate::Status(status) => write_http_response(&mut stream, status, "").await,
                PublishFate::ConnectionClosed | PublishFate::ConnectionRefused => {}
            }
        });
        let manager = SimpleTxManager::from_wallet(
            RootProvider::new_http(url),
            EthereumWallet::from(PrivateKeySigner::random()),
            TxManagerConfig { network_timeout: Duration::from_millis(500), ..Default::default() },
            1,
            Arc::new(NoopTxMetrics),
        )
        .await
        .unwrap();
        let send_state = SendState::new(3).unwrap();
        send_state.set_runtime_mempool_deadline(Duration::from_secs(1));

        let result =
            manager.publish_tx(&send_state, &signed_eip1559().encoded_2718().into(), None).await;

        server.abort();
        if may_have_reached_the_node {
            assert_eq!(result, Err(TxManagerError::NoAnswer));
            assert!(send_state.has_published());
            assert_eq!(send_state.critical_error_at(Duration::from_secs(1)), None);
        } else {
            assert_eq!(result, Err(TxManagerError::Rpc("transport request failed".into())));
            assert!(!send_state.has_published());
            assert_eq!(
                send_state.critical_error_at(Duration::from_secs(1)),
                Some(TxManagerError::MempoolDeadlineExpired)
            );
        }
    }

    #[tokio::test]
    async fn publish_tx_retries_nonce_too_high() {
        let asserter = Asserter::new();
        let envelope = signed_eip1559();
        let raw_tx = envelope.encoded_2718().into();
        let expected_hash = *envelope.tx_hash();
        asserter.push_success(&1u64);
        asserter.push_failure_msg("nonce too high");
        asserter.push_success(&expected_hash);

        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        let wallet = EthereumWallet::from(PrivateKeySigner::random());
        let config = TxManagerConfig {
            publish_max_retries: 2,
            publish_retry_delay: Duration::from_millis(1),
            ..TxManagerConfig::default()
        };
        let manager =
            SimpleTxManager::from_wallet(provider, wallet, config, 1, Arc::new(NoopTxMetrics))
                .await
                .expect("should construct manager");
        let send_state = SendState::new(3).expect("should construct send state");

        let hash = manager
            .publish_tx_with_nonce_retry(&send_state, &raw_tx)
            .await
            .expect("nonce-too-high publication should be retried");

        assert_eq!(hash, expected_hash);
        assert!(send_state.has_published());
        assert!(send_state.critical_error_at(Duration::ZERO).is_none());
    }

    #[tokio::test]
    async fn publish_retry_stops_after_close() {
        let asserter = Asserter::new();
        asserter.push_success(&1u64);
        asserter.push_failure_msg("nonce too high");
        asserter.push_success(&B256::with_last_byte(0x42));

        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        let wallet = EthereumWallet::from(PrivateKeySigner::random());
        let config = TxManagerConfig {
            publish_max_retries: 2,
            publish_retry_delay: Duration::from_millis(20),
            ..TxManagerConfig::default()
        };
        let manager =
            SimpleTxManager::from_wallet(provider, wallet, config, 1, Arc::new(NoopTxMetrics))
                .await
                .expect("should construct manager");
        let send_state = SendState::new(3).expect("should construct send state");

        let closing = manager.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            closing.close();
        });

        let err = manager
            .publish_tx_with_nonce_retry(&send_state, &signed_eip1559().encoded_2718().into())
            .await
            .expect_err("closed manager must not retry publication");
        assert_eq!(err, TxManagerError::ChannelClosed);
        assert!(!send_state.has_published());
    }

    #[test]
    fn publish_retry_uses_injected_runtime_sleep() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let asserter = Asserter::new();
            let envelope = signed_eip1559();
            let raw_tx = envelope.encoded_2718().into();
            let expected_hash = *envelope.tx_hash();
            asserter.push_success(&1u64);
            asserter.push_failure_msg("nonce too high");
            asserter.push_success(&expected_hash);

            let provider = ProviderBuilder::new().connect_mocked_client(asserter);
            let wallet = EthereumWallet::from(PrivateKeySigner::random());
            let config = TxManagerConfig {
                publish_max_retries: 2,
                publish_retry_delay: Duration::from_secs(1),
                ..TxManagerConfig::default()
            };
            let manager = SimpleTxManager::from_wallet_with_runtime(
                ctx.clone(),
                provider,
                wallet,
                config,
                1,
                Arc::new(NoopTxMetrics),
            )
            .await
            .expect("should construct manager");
            let send_state = SendState::new(3).expect("should construct send state");

            let hash = manager
                .publish_tx_with_nonce_retry(&send_state, &raw_tx)
                .await
                .expect("injected runtime should drive the nonce-too-high retry");

            assert_eq!(hash, expected_hash);
            assert_eq!(ctx.now(), Duration::from_secs(1));
            assert!(send_state.has_published());
        });
    }

    #[test]
    fn publish_retry_honors_mempool_deadline() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let asserter = Asserter::new();
            asserter.push_success(&1u64);
            asserter.push_failure_msg("nonce too high");
            asserter.push_success(&B256::with_last_byte(0x42));

            let provider = ProviderBuilder::new().connect_mocked_client(asserter);
            let wallet = EthereumWallet::from(PrivateKeySigner::random());
            let config = TxManagerConfig {
                publish_max_retries: 2,
                publish_retry_delay: Duration::from_secs(2),
                ..TxManagerConfig::default()
            };
            let manager = SimpleTxManager::from_wallet_with_runtime(
                ctx.clone(),
                provider,
                wallet,
                config,
                1,
                Arc::new(NoopTxMetrics),
            )
            .await
            .expect("should construct manager");
            let send_state = SendState::new(3).expect("should construct send state");
            send_state.set_runtime_mempool_deadline(ctx.now() + Duration::from_secs(1));

            let err = manager
                .publish_tx_with_nonce_retry(&send_state, &signed_eip1559().encoded_2718().into())
                .await
                .expect_err("retry must not publish after the mempool deadline");

            assert_eq!(err, TxManagerError::MempoolDeadlineExpired);
            assert!(!send_state.has_published());
        });
    }

    fn decode_eip1559(prepared: &PreparedTx) -> TxEip1559 {
        match prepared.to_envelope().expect("should decode as valid TxEnvelope") {
            TxEnvelope::Eip1559(signed) => signed.strip_signature(),
            other => panic!("expected EIP-1559, got {other:?}"),
        }
    }

    /// A poller keeps polling an unmined transaction and delivers it once it is mined.
    #[test]
    fn receipt_polling_waits_for_the_transaction_to_be_mined() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let tx_hash = B256::with_last_byte(1);
            let (receipt, block) = mined_receipt(tx_hash, 10);

            // Report the transaction unmined for 4 polls, then mined and confirmed at 4 s.
            let asserter = Asserter::new();
            for _ in 0..4 {
                asserter.push_success(&1u64);
                asserter.push_success(&Option::<TransactionReceipt>::None);
            }
            asserter.push_success(&14u64);
            asserter.push_success(&Some(&receipt));
            asserter.push_success(&Some(&block));
            let provider = ProviderBuilder::new().connect_mocked_client(asserter);
            let send_state = SendState::new(3).expect("send state should be valid");
            let config = TxManagerConfig {
                num_confirmations: 5,
                receipt_query_interval: Duration::from_secs(1),
                network_timeout: Duration::from_secs(30),
                ..TxManagerConfig::default()
            };
            let wallet = EthereumWallet::from(PrivateKeySigner::random());
            let nonce_manager = NonceManager::with_runtime(
                ctx.clone(),
                provider.clone(),
                Address::ZERO,
                config.network_timeout,
            );
            let manager = SimpleTxManager {
                provider,
                runtime: ctx.clone(),
                wallet,
                config,
                nonce_manager,
                chain_id: 1,
                closed: CancellationToken::new(),
                metrics: Arc::new(NoopTxMetrics),
                blob_fee_policy: BlobFeePolicy::new(1, 1),
            };

            let confirmed = SimpleTxManager::<_>::wait_mined_using_runtime(
                &manager.runtime,
                &send_state,
                &manager.provider,
                tx_hash,
                &manager.config,
                &manager.closed,
            )
            .await;

            assert_eq!(
                confirmed.map(|confirmed| confirmed.receipt.transaction_hash),
                Some(tx_hash)
            );
            assert_eq!(ctx.now(), Duration::from_secs(4));
        });
    }

    /// Build a successful receipt for `tx_hash` and the canonical block that contains it.
    fn mined_receipt(tx_hash: B256, block_number: u64) -> (TransactionReceipt, Block) {
        let block_hash = B256::with_last_byte(2);
        let receipt = TransactionReceipt {
            inner: ReceiptEnvelope::Legacy(ReceiptWithBloom {
                receipt: Receipt {
                    status: Eip658Value::Eip658(true),
                    cumulative_gas_used: 21_000,
                    logs: vec![],
                },
                logs_bloom: Bloom::ZERO,
            }),
            transaction_hash: tx_hash,
            transaction_index: Some(0),
            block_hash: Some(block_hash),
            block_number: Some(block_number),
            gas_used: 21_000,
            effective_gas_price: 1,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: Some(Address::ZERO),
            contract_address: None,
        };
        let mut block: Block = Block::default();
        block.header.hash = block_hash;

        (receipt, block)
    }

    /// A poller stops querying L1 once the receipt receiver is dropped, as happens when the
    /// send is cancelled, even if its transaction is mined and not yet confirmed.
    #[test]
    fn receipt_polling_stops_once_the_receiver_is_dropped() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let tx_hash = B256::with_last_byte(1);
            let (receipt, block) = mined_receipt(tx_hash, 10);

            // Script a chain tip that never moves, so the mined transaction never reaches the
            // 5 required confirmations.
            let asserter = Asserter::new();
            for _ in 0..8 {
                asserter.push_success(&10u64);
                asserter.push_success(&Some(&receipt));
                asserter.push_success(&Some(&block));
            }
            let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());

            let config = TxManagerConfig {
                num_confirmations: 5,
                receipt_query_interval: Duration::from_secs(1),
                network_timeout: Duration::from_secs(30),
                ..TxManagerConfig::default()
            };
            let nonce_manager = NonceManager::with_runtime(
                ctx.clone(),
                provider.clone(),
                Address::ZERO,
                config.network_timeout,
            );
            let manager = SimpleTxManager {
                provider,
                runtime: ctx.clone(),
                wallet: EthereumWallet::from(PrivateKeySigner::random()),
                config,
                nonce_manager,
                chain_id: 1,
                closed: CancellationToken::new(),
                metrics: Arc::new(NoopTxMetrics),
                blob_fee_policy: BlobFeePolicy::new(1, 1),
            };
            let send_state = Arc::new(SendState::new(3).expect("send state should be valid"));
            let (receipt_tx, receipt_rx) = mpsc::channel(1);

            manager.spawn_wait_for_tx(send_state, tx_hash, receipt_tx);

            // Let the poller see the transaction mined, then drop the receiver.
            ctx.sleep(Duration::from_millis(2500)).await;
            drop(receipt_rx);
            let unused_responses = asserter.read_q().len();

            ctx.sleep(Duration::from_secs(10)).await;
            assert_eq!(asserter.read_q().len(), unused_responses);
        });
    }

    #[rstest]
    #[case::replacement_underpriced("replacement transaction underpriced")]
    #[case::rpc("service unavailable")]
    fn failed_fee_bumps_wait_for_resubmission_timeout(#[case] publish_error: &'static str) {
        Runner::start(Config::seeded(0), move |ctx| async move {
            let resubmission_timeout = Duration::from_secs(10);
            let bump_intervals = 3;
            let rpc_calls_per_bump = 4;
            let asserter = Asserter::new();
            let mut block: Block = Block::default();
            block.header.base_fee_per_gas = Some(1_000_000_000);
            asserter.push_success(&1u64);
            asserter.push_success(&1_000_000_000u128);
            asserter.push_success(&Some(&block));
            asserter.push_success(&21_000u64);
            asserter.push_success(&0u64);
            asserter.push_success(&B256::with_last_byte(1));
            asserter.push_success(&1u64);
            asserter.push_success(&Option::<TransactionReceipt>::None);
            for _ in 0..=bump_intervals {
                asserter.push_success(&1_000_000_000u128);
                asserter.push_success(&Some(&block));
                asserter.push_success(&21_000u64);
                asserter.push_failure_msg(publish_error);
            }
            let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
            let config = TxManagerConfig {
                resubmission_timeout,
                receipt_query_interval: Duration::from_secs(60),
                safe_abort_nonce_too_low_count: 10,
                ..TxManagerConfig::default()
            };
            let manager = SimpleTxManager::from_wallet_with_runtime(
                ctx.clone(),
                provider,
                EthereumWallet::from(PrivateKeySigner::random()),
                config,
                1,
                Arc::new(NoopTxMetrics),
            )
            .await
            .expect("should construct manager");
            let candidate =
                TxCandidate { to: Some(Address::with_last_byte(0x42)), ..Default::default() };
            let mut send = Box::pin(manager.send(candidate));
            assert!(futures::poll!(&mut send).is_pending());
            ctx.sleep(Duration::from_nanos(1)).await;
            assert_eq!(asserter.read_q().len(), (bump_intervals + 1) * rpc_calls_per_bump);

            for attempts in 1..=bump_intervals {
                ctx.sleep(resubmission_timeout - Duration::from_nanos(2)).await;
                assert!(futures::poll!(&mut send).is_pending());
                assert_eq!(
                    asserter.read_q().len(),
                    (bump_intervals + 2 - attempts) * rpc_calls_per_bump,
                    "fee-bump RPC calls must not run before a full resubmission timeout"
                );
                ctx.sleep(Duration::from_nanos(1)).await;
                assert!(futures::poll!(&mut send).is_pending());
                assert_eq!(
                    asserter.read_q().len(),
                    (bump_intervals + 1 - attempts) * rpc_calls_per_bump,
                    "fee-bump RPC calls must run at most once per resubmission interval"
                );
                ctx.sleep(Duration::from_nanos(1)).await;
            }
        });
    }

    /// Counts ERROR events, which tracing exporters use to mark the caller's span failed.
    struct ErrorEventCounter(Arc<AtomicUsize>);

    impl<S: Subscriber> Layer<S> for ErrorEventCounter {
        fn on_event(&self, event: &Event<'_>, _ctx: LayerContext<'_, S>) {
            if *event.metadata().level() == Level::ERROR {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Gas estimation that reverts on the initial send fails the send with an ERROR. When
    /// it reverts on a fee bump, because the published version already executed, the send
    /// still confirms that version and logs no ERROR.
    #[rstest]
    #[case::initial_send(false)]
    #[case::fee_bump(true)]
    fn gas_estimation_revert_is_an_error_only_on_the_initial_send(#[case] on_fee_bump: bool) {
        let errors = Arc::new(AtomicUsize::new(0));
        let subscriber = Registry::default().with(ErrorEventCounter(Arc::clone(&errors)));
        let result = tracing::subscriber::with_default(subscriber, || {
            Runner::start(Config::seeded(0), move |ctx| async move {
                let tx_hash = B256::with_last_byte(1);
                let (receipt, block) = mined_receipt(tx_hash, 10);
                let mut latest: Block = Block::default();
                latest.header.base_fee_per_gas = Some(1_000_000_000);
                let asserter = Asserter::new();
                asserter.push_success(&1u64);
                asserter.push_success(&1_000_000_000u128);
                asserter.push_success(&Some(&latest));
                if on_fee_bump {
                    asserter.push_success(&21_000u64);
                    asserter.push_success(&0u64);
                    asserter.push_success(&tx_hash);
                    // The receipt poller misses the transaction before the bump at 10 s.
                    asserter.push_success(&9u64);
                    asserter.push_success(&Option::<TransactionReceipt>::None);
                    asserter.push_success(&1_000_000_000u128);
                    asserter.push_success(&Some(&latest));
                    asserter.push_failure_msg("execution reverted");
                    // It sees the transaction mined on its next poll at 15 s.
                    asserter.push_success(&10u64);
                    asserter.push_success(&Some(&receipt));
                    asserter.push_success(&Some(&block));
                } else {
                    asserter.push_failure_msg("execution reverted");
                }
                let manager = SimpleTxManager::from_wallet_with_runtime(
                    ctx.clone(),
                    ProviderBuilder::new().connect_mocked_client(asserter.clone()),
                    EthereumWallet::from(PrivateKeySigner::random()),
                    TxManagerConfig {
                        num_confirmations: 1,
                        resubmission_timeout: Duration::from_secs(10),
                        receipt_query_interval: Duration::from_secs(15),
                        ..TxManagerConfig::default()
                    },
                    1,
                    Arc::new(NoopTxMetrics),
                )
                .await
                .expect("should construct manager");

                let result = manager
                    .send(TxCandidate {
                        to: Some(Address::with_last_byte(0x42)),
                        ..Default::default()
                    })
                    .await;

                assert!(asserter.read_q().is_empty(), "every mocked response should be used");
                result.map(|receipt| receipt.transaction_hash)
            })
        });

        if on_fee_bump {
            assert_eq!(result, Ok(B256::with_last_byte(1)));
            assert_eq!(errors.load(Ordering::Relaxed), 0, "a bump revert must not log an error");
        } else {
            assert_eq!(result, Err(TxManagerError::ExecutionReverted { reason: None, data: None }));
            assert_eq!(errors.load(Ordering::Relaxed), 1, "an initial revert must log an error");
        }
    }

    /// Closing the manager ends an in-flight send that is waiting for a receipt or the next
    /// fee bump right away, instead of after the resubmission timeout.
    #[test]
    fn close_ends_a_send_waiting_for_its_receipt() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let mut block: Block = Block::default();
            block.header.base_fee_per_gas = Some(1_000_000_000);
            let asserter = Asserter::new();
            asserter.push_success(&1u64);
            asserter.push_success(&1_000_000_000u128);
            asserter.push_success(&Some(&block));
            asserter.push_success(&21_000u64);
            asserter.push_success(&0u64);
            asserter.push_success(&B256::with_last_byte(1));
            asserter.push_success(&1u64);
            asserter.push_success(&Option::<TransactionReceipt>::None);
            let config = TxManagerConfig {
                resubmission_timeout: Duration::from_secs(48),
                receipt_query_interval: Duration::from_secs(60),
                ..TxManagerConfig::default()
            };
            let manager = SimpleTxManager::from_wallet_with_runtime(
                ctx.clone(),
                ProviderBuilder::new().connect_mocked_client(asserter),
                EthereumWallet::from(PrivateKeySigner::random()),
                config,
                1,
                Arc::new(NoopTxMetrics),
            )
            .await
            .expect("should construct manager");
            let mut send = Box::pin(manager.send(TxCandidate {
                to: Some(Address::with_last_byte(0x42)),
                ..Default::default()
            }));
            assert!(futures::poll!(&mut send).is_pending());
            ctx.sleep(Duration::from_secs(1)).await;
            assert!(futures::poll!(&mut send).is_pending());

            manager.close();

            assert_eq!(futures::poll!(&mut send), Poll::Ready(Err(TxManagerError::ChannelClosed)));
        });
    }

    /// A confirmed send records its inclusion latency together with its send latency,
    /// from the timestamp of the canonical block the receipt poller already read: the
    /// latency runs from the start of the send, in milliseconds of wall-clock time, to
    /// that timestamp, and no other block is read.
    #[test]
    fn confirmed_send_records_inclusion_latency_with_send_latency() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let tx_hash = B256::with_last_byte(1);
            let (receipt, mut block) = mined_receipt(tx_hash, 10);
            block.header.inner.timestamp = 1_700_000_020;
            let mut latest: Block = Block::default();
            latest.header.base_fee_per_gas = Some(1_000_000_000);
            let asserter = Asserter::new();
            asserter.push_success(&1u64);
            asserter.push_success(&1_000_000_000u128);
            asserter.push_success(&Some(&latest));
            asserter.push_success(&21_000u64);
            asserter.push_success(&0u64);
            asserter.push_success(&tx_hash);
            asserter.push_success(&10u64);
            asserter.push_success(&Some(&receipt));
            asserter.push_success(&Some(&block));
            let mut metrics = MockTxMetrics::new();
            metrics
                .expect_record_send_latency()
                .with(eq(0), eq(SendOutcome::Confirmed))
                .times(1)
                .return_const(());
            // The send starts at 1_700_000_000.300 s and the block's timestamp is 1_700_000_020 s.
            metrics.expect_record_inclusion_latency().with(eq(19_700)).times(1).return_const(());
            // The send also records fee, nonce and outcome metrics, which this test does not check.
            metrics.expect_record_basefee().return_const(());
            metrics.expect_record_tipcap().return_const(());
            metrics.expect_record_tx_max_fee().return_const(());
            metrics.expect_record_current_nonce().return_const(());
            metrics.expect_record_tx_confirmed().return_const(());
            let manager = SimpleTxManager::from_wallet_with_runtime(
                ctx.clone(),
                ProviderBuilder::new().connect_mocked_client(asserter.clone()),
                EthereumWallet::from(PrivateKeySigner::random()),
                TxManagerConfig { num_confirmations: 1, ..TxManagerConfig::default() },
                1,
                Arc::new(metrics),
            )
            .await
            .expect("should construct manager");
            ctx.sleep(Duration::from_millis(300)).await;

            let confirmed = manager
                .send(TxCandidate { to: Some(Address::with_last_byte(0x42)), ..Default::default() })
                .await
                .expect("the send should confirm");

            assert_eq!(confirmed.transaction_hash, tx_hash);
            assert!(asserter.read_q().is_empty(), "every mocked response should be used");
        });
    }

    // ── blob fee policy ────────────────────────────────────────────────

    const BLOB_RESUBMISSION_TIMEOUT: Duration = Duration::from_secs(10);

    /// A one-blob candidate.
    fn blob_candidate() -> TxCandidate {
        TxCandidate {
            to: Some(Address::with_last_byte(0x42)),
            blobs: Arc::from(vec![Box::<Blob>::default()]),
            ..Default::default()
        }
    }

    /// Pushes the responses that price and sign a blob tx version: its L1 prices (the latest
    /// block, with a 1 gwei base fee, and a 1 gwei blob base fee) and its gas estimate.
    fn push_blob_pricing(asserter: &Asserter) {
        let mut block: Block = Block::default();
        block.header.base_fee_per_gas = Some(1_000_000_000);
        asserter.push_success(&Some(&block));
        asserter.push_success(&1_000_000_000u128);
        asserter.push_success(&21_000u64);
    }

    /// Metrics that accept every record a blob send makes, except the landed blob tx and the
    /// fee bumps, which each test sets.
    fn blob_send_metrics() -> MockTxMetrics {
        let mut metrics = MockTxMetrics::new();
        metrics.expect_record_basefee().return_const(());
        metrics.expect_record_tipcap().return_const(());
        metrics.expect_record_blob_fee_cap().return_const(());
        metrics.expect_record_tx_max_fee().return_const(());
        metrics.expect_record_current_nonce().return_const(());
        metrics.expect_record_publish_error().return_const(());
        metrics.expect_record_rpc_error().return_const(());
        metrics.expect_record_send_latency().return_const(());
        metrics.expect_record_inclusion_latency().return_const(());
        metrics.expect_record_tx_confirmed().return_const(());
        metrics
    }

    /// A mocked provider over `asserter`.
    fn mocked(asserter: &Asserter) -> RootProvider {
        ProviderBuilder::default().connect_mocked_client(asserter.clone())
    }

    /// A manager over `provider`, whose first response must be the chain ID, with one
    /// confirmation, a 60 s receipt polling interval and the given blob bounds, in wei.
    async fn blob_manager<P: Provider + Clone + std::fmt::Debug + 'static>(
        ctx: &Context,
        provider: P,
        bounds: (u128, u128),
        metrics: MockTxMetrics,
    ) -> SimpleTxManager<P, Context> {
        SimpleTxManager::from_wallet_with_runtime(
            ctx.clone(),
            provider,
            EthereumWallet::from(PrivateKeySigner::random()),
            TxManagerConfig {
                num_confirmations: 1,
                resubmission_timeout: BLOB_RESUBMISSION_TIMEOUT,
                receipt_query_interval: Duration::from_secs(60),
                safe_abort_nonce_too_low_count: 10,
                max_priority_fee_per_gas_per_blob_lower_bound: bounds.0,
                max_priority_fee_per_gas_per_blob_upper_bound: bounds.1,
                ..TxManagerConfig::default()
            },
            1,
            Arc::new(metrics),
        )
        .await
        .expect("should construct manager")
    }

    /// A provider over a mocked client that answers receipt queries itself, under the hashes
    /// the manager signed: the version published at position `landing` (from 0) is mined at
    /// block 10 once a replacement has been published, and every other version is pending.
    /// Versions are numbered in the order their receipts are first polled, which is the
    /// order they are published in.
    #[derive(Debug, Clone)]
    struct LandingProvider {
        inner: RootProvider,
        landing: usize,
        polled: Arc<Mutex<Vec<B256>>>,
    }

    impl Provider for LandingProvider {
        fn root(&self) -> &RootProvider {
            &self.inner
        }

        fn get_transaction_receipt(
            &self,
            hash: B256,
        ) -> ProviderCall<(B256,), Option<TransactionReceipt>> {
            let mut polled = self.polled.lock().unwrap();
            if !polled.contains(&hash) {
                polled.push(hash);
            }
            let mined = polled.len() > 1 && polled[self.landing] == hash;
            ProviderCall::ready(Ok(mined.then(|| mined_receipt(hash, 10).0)))
        }
    }

    /// A blob tx still pending after the resubmission timeout is replaced with doubled fees,
    /// which raises the fees the next blob txs start from. Whichever version lands, the
    /// replacement or the first version, the next blob tx, here with two blobs, is then
    /// signed at 90% of the `max_priority_fee_per_gas` per blob that version paid, and the
    /// metrics record that value and the replacement.
    #[rstest]
    #[case::the_replacement_lands(1, 0.02, 18_000_000)]
    #[case::the_first_version_lands(0, 0.01, 10_000_000)]
    fn a_stuck_blob_tx_is_replaced_on_the_timer_and_the_landed_fees_carry_to_the_next_one(
        #[case] landing: usize,
        #[case] landed_max_priority_fee_per_gas_per_blob_gwei: f64,
        #[case] next_max_priority_fee_per_gas_per_blob: u128,
    ) {
        Runner::start(Config::seeded(0), move |ctx| async move {
            // The receipt of each version is polled when it is published, and again 15 s later.
            let receipt_query_interval = BLOB_RESUBMISSION_TIMEOUT + Duration::from_secs(5);
            let (_, block) = mined_receipt(B256::ZERO, 10);
            let asserter = Asserter::new();
            asserter.push_success(&1u64);
            push_blob_pricing(&asserter);
            asserter.push_success(&0u64);
            asserter.push_success(&B256::ZERO);
            asserter.push_success(&1u64);
            // The replacement, signed at the resubmission timeout, and its first poll.
            push_blob_pricing(&asserter);
            asserter.push_success(&B256::ZERO);
            asserter.push_success(&10u64);
            if landing == 0 {
                // The first version, polled again.
                asserter.push_success(&10u64);
            }
            // The canonical block of the landed version.
            asserter.push_success(&Some(&block));
            let mut metrics = blob_send_metrics();
            metrics.expect_record_gas_bump().times(1).return_const(());
            metrics
                .expect_record_blob_tx_landed()
                .with(eq(landed_max_priority_fee_per_gas_per_blob_gwei), eq(1))
                .times(1)
                .return_const(());
            let polled = Arc::new(Mutex::new(Vec::new()));
            let provider =
                LandingProvider { inner: mocked(&asserter), landing, polled: Arc::clone(&polled) };
            let mut manager =
                blob_manager(&ctx, provider, (10_000_000, 10_000_000_000), metrics).await;
            manager.config.receipt_query_interval = receipt_query_interval;
            let mut send = Box::pin(manager.send(blob_candidate()));
            assert!(futures::poll!(&mut send).is_pending());
            ctx.sleep(BLOB_RESUBMISSION_TIMEOUT - Duration::from_nanos(1)).await;
            assert_eq!(manager.blob_fee_policy.max_priority_fee_per_gas_per_blob(), 10_000_000);
            if landing == 0 {
                // Published, the replacement raises the next blob txs before anything lands.
                ctx.sleep(Duration::from_nanos(2)).await;
                assert!(futures::poll!(&mut send).is_pending());
                assert_eq!(manager.blob_fee_policy.max_priority_fee_per_gas_per_blob(), 20_000_000);
            }

            let confirmed = send.await.expect("the send should confirm");

            let polled = polled.lock().unwrap().clone();
            assert_eq!(polled.len(), 2, "the first version and its replacement are published");
            assert_eq!(confirmed.transaction_hash, polled[landing]);
            assert_eq!(
                ctx.now(),
                if landing == 0 { receipt_query_interval } else { BLOB_RESUBMISSION_TIMEOUT }
            );

            push_blob_pricing(&asserter);
            let next = TxCandidate {
                blobs: Arc::from(vec![Box::<Blob>::default(), Box::<Blob>::default()]),
                ..blob_candidate()
            };
            let next = manager.craft_tx(&next, None).await.expect("should craft the next blob tx");
            let next = next.to_envelope().unwrap();
            let next = next.as_eip4844().expect("should be EIP-4844").tx().tx();
            let next_max_priority_fee_per_gas = 2 * next_max_priority_fee_per_gas_per_blob;
            assert_eq!(next.max_priority_fee_per_gas, next_max_priority_fee_per_gas);
            assert_eq!(next.max_fee_per_gas, next_max_priority_fee_per_gas + 2_000_000_000);
            assert_eq!(next.max_fee_per_blob_gas, 2_000_000_000);
            assert!(asserter.read_q().is_empty(), "every mocked response should be used");
        });
    }

    /// A first blob tx version the node refuses for its fees, as when a version signed
    /// before a restart holds its nonce or its priority fee is below the node's minimum, is
    /// replaced right away instead of failing the send.
    #[rstest]
    #[case::replacement_underpriced("replacement transaction underpriced")]
    #[case::below_minimum_price(
        "transaction gas price below minimum: gas tip cap 10000000, minimum needed 15000000"
    )]
    fn a_first_blob_tx_refused_for_its_fees_is_replaced_right_away(
        #[case] rejection: &'static str,
    ) {
        Runner::start(Config::seeded(0), move |ctx| async move {
            let tx_hash = B256::with_last_byte(1);
            let (receipt, block) = mined_receipt(tx_hash, 10);
            let asserter = Asserter::new();
            asserter.push_success(&1u64);
            push_blob_pricing(&asserter);
            asserter.push_success(&0u64);
            asserter.push_failure_msg(rejection);
            push_blob_pricing(&asserter);
            asserter.push_success(&tx_hash);
            asserter.push_success(&10u64);
            asserter.push_success(&Some(&receipt));
            asserter.push_success(&Some(&block));
            let mut metrics = blob_send_metrics();
            metrics.expect_record_gas_bump().never();
            metrics.expect_record_blob_tx_landed().with(eq(0.02), eq(0)).times(1).return_const(());
            let manager =
                blob_manager(&ctx, mocked(&asserter), (10_000_000, 10_000_000_000), metrics).await;

            manager.send(blob_candidate()).await.expect("the send should confirm");

            assert_eq!(ctx.now(), Duration::ZERO);
            assert!(asserter.read_q().is_empty(), "every mocked response should be used");
        });
    }

    /// A blob tx replacement the node refuses for its fees, as when a version whose
    /// publication got no answer reached the mempool, is replaced right away from the
    /// refused fees.
    #[test]
    fn a_blob_replacement_refused_for_its_fees_is_replaced_right_away() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let tx_hash = B256::with_last_byte(3);
            let (receipt, block) = mined_receipt(tx_hash, 10);
            let asserter = Asserter::new();
            asserter.push_success(&1u64);
            push_blob_pricing(&asserter);
            asserter.push_success(&0u64);
            asserter.push_success(&B256::with_last_byte(1));
            asserter.push_success(&1u64);
            asserter.push_success(&Option::<TransactionReceipt>::None);
            // At the resubmission timeout, the replacement is refused and replaced again.
            push_blob_pricing(&asserter);
            asserter.push_failure_msg("replacement transaction underpriced");
            push_blob_pricing(&asserter);
            asserter.push_success(&tx_hash);
            asserter.push_success(&10u64);
            asserter.push_success(&Some(&receipt));
            asserter.push_success(&Some(&block));
            let mut metrics = blob_send_metrics();
            metrics.expect_record_gas_bump().times(1).return_const(());
            metrics.expect_record_blob_tx_landed().with(eq(0.04), eq(1)).times(1).return_const(());
            let manager =
                blob_manager(&ctx, mocked(&asserter), (10_000_000, 10_000_000_000), metrics).await;

            manager.send(blob_candidate()).await.expect("the send should confirm");

            assert_eq!(ctx.now(), BLOB_RESUBMISSION_TIMEOUT);
            assert!(asserter.read_q().is_empty(), "every mocked response should be used");
            assert_eq!(manager.blob_fee_policy.max_priority_fee_per_gas_per_blob(), 36_000_000);
        });
    }

    /// A first blob tx version the node already knows, as when the same signed version was
    /// sent before without an answer, is published as it is: no fee is raised.
    #[test]
    fn a_first_blob_tx_already_known_is_published() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let tx_hash = B256::with_last_byte(1);
            let (receipt, block) = mined_receipt(tx_hash, 10);
            let asserter = Asserter::new();
            asserter.push_success(&1u64);
            push_blob_pricing(&asserter);
            asserter.push_success(&0u64);
            asserter.push_failure_msg("already known");
            asserter.push_success(&10u64);
            asserter.push_success(&Some(&receipt));
            asserter.push_success(&Some(&block));
            let mut metrics = blob_send_metrics();
            metrics.expect_record_gas_bump().never();
            metrics.expect_record_blob_tx_landed().with(eq(0.01), eq(0)).times(1).return_const(());
            let manager =
                blob_manager(&ctx, mocked(&asserter), (10_000_000, 10_000_000_000), metrics).await;

            manager.send(blob_candidate()).await.expect("the send should confirm");

            assert!(asserter.read_q().is_empty(), "every mocked response should be used");
        });
    }

    /// A blob tx version whose publication fails with an RPC error, such as a network error,
    /// is sent again as it is after the publish retry delay: nothing is signed again and no
    /// fee is raised. This holds for a first version and for a version just escalated after a
    /// fee refusal.
    #[rstest]
    #[case::first_version(false, 0.01)]
    #[case::escalated_version(true, 0.02)]
    fn a_blob_tx_publication_failing_on_the_network_resends_the_same_version(
        #[case] escalated: bool,
        #[case] landed_max_priority_fee_per_gas_per_blob_gwei: f64,
    ) {
        Runner::start(Config::seeded(0), move |ctx| async move {
            let tx_hash = B256::with_last_byte(1);
            let (receipt, block) = mined_receipt(tx_hash, 10);
            let asserter = Asserter::new();
            asserter.push_success(&1u64);
            push_blob_pricing(&asserter);
            asserter.push_success(&0u64);
            if escalated {
                asserter.push_failure_msg("replacement transaction underpriced");
                push_blob_pricing(&asserter);
            }
            asserter.push_failure_msg("connection reset by peer");
            asserter.push_success(&tx_hash);
            asserter.push_success(&10u64);
            asserter.push_success(&Some(&receipt));
            asserter.push_success(&Some(&block));
            let mut metrics = blob_send_metrics();
            metrics.expect_record_gas_bump().never();
            metrics
                .expect_record_blob_tx_landed()
                .with(eq(landed_max_priority_fee_per_gas_per_blob_gwei), eq(0))
                .times(1)
                .return_const(());
            let manager =
                blob_manager(&ctx, mocked(&asserter), (10_000_000, 10_000_000_000), metrics).await;

            manager.send(blob_candidate()).await.expect("the send should confirm");

            assert_eq!(ctx.now(), manager.config.publish_retry_delay);
            assert!(asserter.read_q().is_empty(), "every mocked response should be used");
        });
    }

    /// A blob tx replacement sent again after a network error, which the node then already
    /// knows, is tracked under its own hash, so it is found when it lands.
    #[test]
    fn a_blob_replacement_already_known_on_resend_is_tracked_under_its_own_hash() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (_, block) = mined_receipt(B256::ZERO, 10);
            let asserter = Asserter::new();
            asserter.push_success(&1u64);
            push_blob_pricing(&asserter);
            asserter.push_success(&0u64);
            asserter.push_success(&B256::ZERO);
            asserter.push_success(&1u64);
            // The replacement, signed at the resubmission timeout, reaches the node although
            // its publication fails, and its first poll finds it mined.
            push_blob_pricing(&asserter);
            asserter.push_failure_msg("connection reset by peer");
            asserter.push_failure_msg("already known");
            asserter.push_success(&10u64);
            asserter.push_success(&Some(&block));
            let mut metrics = blob_send_metrics();
            metrics.expect_record_gas_bump().times(1).return_const(());
            metrics.expect_record_blob_tx_landed().with(eq(0.02), eq(1)).times(1).return_const(());
            let polled = Arc::new(Mutex::new(Vec::new()));
            let provider = LandingProvider {
                inner: mocked(&asserter),
                landing: 1,
                polled: Arc::clone(&polled),
            };
            let manager = blob_manager(&ctx, provider, (10_000_000, 10_000_000_000), metrics).await;

            // Polled under the hash of the first version only, the send would never confirm.
            let confirmed = tokio::select! {
                result = manager.send(blob_candidate()) => result.expect("the send should confirm"),
                () = ctx.sleep(3 * BLOB_RESUBMISSION_TIMEOUT) => panic!("the replacement should land"),
            };

            let polled = polled.lock().unwrap().clone();
            assert_eq!(polled.len(), 2, "the replacement is polled under its own hash");
            assert_eq!(confirmed.transaction_hash, polled[1]);
            assert!(asserter.read_q().is_empty(), "every mocked response should be used");
        });
    }

    /// Transport over a mocked client that loses the answers to the publishes at the
    /// positions (from 0) in `lost`, as when the connection breaks after the node got the
    /// transaction: they fail in transport and use no mocked response.
    #[derive(Debug, Clone)]
    struct LosingPublishAnswers {
        inner: MockTransport,
        lost: &'static [usize],
        publishes: Arc<AtomicUsize>,
    }

    impl LosingPublishAnswers {
        /// A provider over `asserter` that loses the answers to the publishes in `lost`.
        fn provider(asserter: &Asserter, lost: &'static [usize]) -> RootProvider {
            let transport = Self {
                inner: MockTransport::new(asserter.clone()),
                lost,
                publishes: Arc::default(),
            };
            RootProvider::new(RpcClient::new(transport, true))
        }
    }

    impl tower::Service<RequestPacket> for LosingPublishAnswers {
        type Response = ResponsePacket;
        type Error = TransportError;
        type Future = TransportFut<'static>;

        fn poll_ready(&mut self, cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        fn call(&mut self, request: RequestPacket) -> Self::Future {
            if request.method_names().any(|method| method == "eth_sendRawTransaction")
                && self.lost.contains(&self.publishes.fetch_add(1, Ordering::SeqCst))
            {
                return Box::pin(async { Err(TransportErrorKind::custom_str("connection reset")) });
            }
            self.inner.call(request)
        }
    }

    /// A first blob tx version whose publication got no answer is sent again as it is after
    /// the publish retry delay. Whatever the node then answers, the send follows that version,
    /// which may be in a mempool: it confirms when the version lands, and no fee is raised.
    /// A fee refusal, as from a load-balanced backend that never saw the version, does not
    /// replace it, or its receipt would no longer be polled.
    #[rstest]
    #[case::accepted(None)]
    #[case::already_known(Some("already known"))]
    #[case::nonce_too_low(Some("nonce too low"))]
    #[case::refused_for_its_fees(Some("replacement transaction underpriced"))]
    fn a_blob_tx_whose_publication_got_no_answer_is_resent_and_followed(
        #[case] resend_error: Option<&'static str>,
    ) {
        Runner::start(Config::seeded(0), move |ctx| async move {
            let tx_hash = B256::with_last_byte(1);
            let (receipt, block) = mined_receipt(tx_hash, 10);
            let asserter = Asserter::new();
            asserter.push_success(&1u64);
            push_blob_pricing(&asserter);
            asserter.push_success(&0u64);
            match resend_error {
                None => asserter.push_success(&tx_hash),
                Some(error) => asserter.push_failure_msg(error),
            }
            asserter.push_success(&10u64);
            asserter.push_success(&Some(&receipt));
            asserter.push_success(&Some(&block));
            let mut metrics = blob_send_metrics();
            metrics.expect_record_gas_bump().never();
            metrics.expect_record_blob_tx_landed().with(eq(0.01), eq(0)).times(1).return_const(());
            let manager = blob_manager(
                &ctx,
                LosingPublishAnswers::provider(&asserter, &[0]),
                (10_000_000, 10_000_000_000),
                metrics,
            )
            .await;

            manager.send(blob_candidate()).await.expect("the send should confirm");

            assert_eq!(ctx.now(), manager.config.publish_retry_delay);
            assert!(asserter.read_q().is_empty(), "every mocked response should be used");
        });
    }

    /// A first blob tx version whose publications never get an answer is followed as
    /// published once its resends are used up: the send confirms when it lands.
    #[test]
    fn a_blob_tx_whose_publications_never_get_an_answer_is_followed() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (receipt, block) = mined_receipt(B256::with_last_byte(1), 10);
            let asserter = Asserter::new();
            asserter.push_success(&1u64);
            push_blob_pricing(&asserter);
            asserter.push_success(&0u64);
            asserter.push_success(&10u64);
            asserter.push_success(&Some(&receipt));
            asserter.push_success(&Some(&block));
            let mut metrics = blob_send_metrics();
            metrics.expect_record_gas_bump().never();
            metrics.expect_record_blob_tx_landed().with(eq(0.01), eq(0)).times(1).return_const(());
            let mut manager = blob_manager(
                &ctx,
                LosingPublishAnswers::provider(&asserter, &[0, 1]),
                (10_000_000, 10_000_000_000),
                metrics,
            )
            .await;
            manager.config.publish_max_retries = 1;

            manager.send(blob_candidate()).await.expect("the send should confirm");

            assert_eq!(ctx.now(), manager.config.publish_retry_delay);
            assert!(asserter.read_q().is_empty(), "every mocked response should be used");
        });
    }

    /// A fee bump whose publication got no answer is followed as published: its receipt is
    /// polled under its own hash, so the send confirms when it lands.
    #[test]
    fn a_fee_bump_whose_publication_got_no_answer_is_followed() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let mut block: Block = Block::default();
            block.header.base_fee_per_gas = Some(1_000_000_000);
            let asserter = Asserter::new();
            asserter.push_success(&1u64);
            asserter.push_success(&1_000_000_000u128);
            asserter.push_success(&Some(&block));
            asserter.push_success(&21_000u64);
            asserter.push_success(&0u64);
            asserter.push_success(&B256::ZERO);
            asserter.push_success(&1u64);
            // The fee bump, priced at the resubmission timeout, whose answer is lost.
            asserter.push_success(&1_000_000_000u128);
            asserter.push_success(&Some(&block));
            asserter.push_success(&21_000u64);
            asserter.push_success(&10u64);
            // The canonical block of the fee bump.
            asserter.push_success(&Some(&mined_receipt(B256::ZERO, 10).1));
            let mut metrics = blob_send_metrics();
            metrics.expect_record_gas_bump().times(1).return_const(());
            let polled = Arc::new(Mutex::new(Vec::new()));
            let provider = LandingProvider {
                inner: LosingPublishAnswers::provider(&asserter, &[1]),
                landing: 1,
                polled: Arc::clone(&polled),
            };
            let manager = blob_manager(&ctx, provider, (10_000_000, 10_000_000_000), metrics).await;
            let candidate =
                TxCandidate { to: Some(Address::with_last_byte(0x42)), ..Default::default() };

            // Polled under the hash of the first version only, the send would never confirm.
            let confirmed = tokio::select! {
                result = manager.send(candidate) => result.expect("the send should confirm"),
                () = ctx.sleep(3 * BLOB_RESUBMISSION_TIMEOUT) => panic!("the fee bump should land"),
            };

            let polled = polled.lock().unwrap().clone();
            assert_eq!(polled.len(), 2, "the fee bump is polled under its own hash");
            assert_eq!(confirmed.transaction_hash, polled[1]);
            assert_eq!(ctx.now(), BLOB_RESUBMISSION_TIMEOUT);
            assert!(asserter.read_q().is_empty(), "every mocked response should be used");
        });
    }

    /// A first blob tx version whose publication fails on the network is not sent again once
    /// the mempool deadline expired during the publish retry delay: the send fails.
    #[test]
    fn a_blob_tx_is_not_resent_after_the_mempool_deadline() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let asserter = Asserter::new();
            asserter.push_success(&1u64);
            push_blob_pricing(&asserter);
            asserter.push_success(&0u64);
            asserter.push_failure_msg("connection reset by peer");
            let mut metrics = blob_send_metrics();
            metrics.expect_record_tx_failed().times(1).return_const(());
            let mut manager =
                blob_manager(&ctx, mocked(&asserter), (10_000_000, 10_000_000_000), metrics).await;
            manager.config.tx_not_in_mempool_timeout = Duration::from_secs(1);
            manager.config.publish_retry_delay = Duration::from_secs(2);

            let err = manager.send(blob_candidate()).await.expect_err("the send should fail");

            assert_eq!(err, TxManagerError::MempoolDeadlineExpired);
            assert_eq!(ctx.now(), Duration::from_secs(2));
            assert!(asserter.read_q().is_empty(), "every mocked response should be used");
        });
    }

    /// A blob tx version whose publication fails on the network is not sent again once the
    /// manager is closed during the publish retry delay. This holds for a first version and
    /// for a replacement.
    #[rstest]
    #[case::first_version(false)]
    #[case::replacement(true)]
    fn a_blob_tx_is_not_resent_after_the_manager_is_closed(#[case] replacement: bool) {
        Runner::start(Config::seeded(0), move |ctx| async move {
            let asserter = Asserter::new();
            asserter.push_success(&1u64);
            push_blob_pricing(&asserter);
            asserter.push_success(&0u64);
            if replacement {
                asserter.push_success(&B256::with_last_byte(1));
                asserter.push_success(&1u64);
                asserter.push_success(&Option::<TransactionReceipt>::None);
                // The replacement, signed at the resubmission timeout.
                push_blob_pricing(&asserter);
            }
            asserter.push_failure_msg("connection reset by peer");
            // The answer a resend would get.
            asserter.push_success(&B256::with_last_byte(2));
            let mut metrics = blob_send_metrics();
            metrics.expect_record_gas_bump().never();
            metrics.expect_record_tx_failed().times(1).return_const(());
            let manager =
                blob_manager(&ctx, mocked(&asserter), (10_000_000, 10_000_000_000), metrics).await;
            let mut send = Box::pin(manager.send(blob_candidate()));
            assert!(futures::poll!(&mut send).is_pending());
            if replacement {
                ctx.sleep(BLOB_RESUBMISSION_TIMEOUT + Duration::from_nanos(1)).await;
                assert!(futures::poll!(&mut send).is_pending());
            }
            assert_eq!(asserter.read_q().len(), 1, "the failed publication should be sent");

            manager.close();

            assert_eq!(
                send.await.expect_err("the send should fail"),
                TxManagerError::ChannelClosed
            );
            assert_eq!(asserter.read_q().len(), 1, "nothing should be sent again");
        });
    }

    /// A blob tx whose doubled `max_priority_fee_per_gas` would pass the upper bound is not
    /// replaced: at each resubmission timeout, nothing is read, signed or published.
    #[test]
    fn a_blob_tx_at_the_upper_bound_is_not_replaced() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let asserter = Asserter::new();
            asserter.push_success(&1u64);
            push_blob_pricing(&asserter);
            asserter.push_success(&0u64);
            asserter.push_success(&B256::with_last_byte(1));
            asserter.push_success(&1u64);
            asserter.push_success(&Option::<TransactionReceipt>::None);
            // What a replacement would read first.
            push_blob_pricing(&asserter);
            let mut metrics = blob_send_metrics();
            metrics.expect_record_gas_bump().never();
            let manager =
                blob_manager(&ctx, mocked(&asserter), (1_000_000_000, 1_000_000_000), metrics)
                    .await;
            let mut send = Box::pin(manager.send(blob_candidate()));
            assert!(futures::poll!(&mut send).is_pending());

            for _ in 0..3 {
                ctx.sleep(BLOB_RESUBMISSION_TIMEOUT + Duration::from_nanos(1)).await;
                assert!(futures::poll!(&mut send).is_pending());
            }

            assert_eq!(asserter.read_q().len(), 3, "no replacement should be signed");
            assert_eq!(manager.blob_fee_policy.max_priority_fee_per_gas_per_blob(), 1_000_000_000);
        });
    }

    /// A first blob tx version the node refuses for its fees, and whose replacement would
    /// pass the upper bound, fails the send: nothing was published, so no receipt can come.
    #[test]
    fn a_first_blob_tx_refused_for_its_fees_at_the_upper_bound_fails_the_send() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let asserter = Asserter::new();
            asserter.push_success(&1u64);
            push_blob_pricing(&asserter);
            asserter.push_success(&0u64);
            asserter.push_failure_msg("replacement transaction underpriced");
            let mut metrics = blob_send_metrics();
            metrics.expect_record_gas_bump().never();
            metrics.expect_record_blob_tx_landed().never();
            metrics.expect_record_tx_failed().times(1).return_const(());
            let manager =
                blob_manager(&ctx, mocked(&asserter), (1_000_000_000, 1_000_000_000), metrics)
                    .await;

            let result = manager.send(blob_candidate()).await;

            assert_eq!(
                result.map(|receipt| receipt.transaction_hash),
                Err(TxManagerError::FeeLimitExceeded {
                    fee: 2_000_000_000,
                    ceiling: 1_000_000_000
                })
            );
            assert_eq!(ctx.now(), Duration::ZERO);
            assert!(asserter.read_q().is_empty(), "every mocked response should be used");
        });
    }

    // ── should_reset_nonce_on_send_error ───────────────────────────────

    #[rstest]
    #[case::error_before_first_publish(false, Err(TxManagerError::SendTimeout), None, true)]
    #[case::non_timeout_error_after_publish(true, Err(TxManagerError::ChannelClosed), None, false)]
    #[case::timeout_after_publish(true, Err(TxManagerError::SendTimeout), None, true)]
    #[case::success(false, Ok(()), None, false)]
    #[case::nonce_too_high_no_override(false, Err(TxManagerError::NonceTooHigh), None, true)]
    #[case::nonce_too_high_with_override(false, Err(TxManagerError::NonceTooHigh), Some(42), false)]
    #[case::nonce_override_timeout(false, Err(TxManagerError::SendTimeout), Some(42), false)]
    #[case::nonce_override_pre_publish_error(
        false,
        Err(TxManagerError::ChannelClosed),
        Some(42),
        false
    )]
    fn should_reset_nonce_on_send_error(
        #[case] has_publish: bool,
        #[case] result: crate::TxManagerResult<()>,
        #[case] nonce_override: Option<u64>,
        #[case] expected: bool,
    ) {
        let send_state = crate::SendState::new(3).expect("should create send state");
        if has_publish {
            // Publishing is only reachable once a nonce has been signed.
            send_state.record_assigned_nonce(0);
            send_state.record_publish();
        }
        assert_eq!(
            SimpleTxManager::<RootProvider>::should_reset_nonce_on_send_error(
                &result,
                &send_state,
                nonce_override,
            ),
            expected,
        );
    }

    #[test]
    fn signed_send_nonce_is_recycled_instead_of_resetting_the_cache() {
        let send_state = crate::SendState::new(3).expect("should create send state");
        send_state.record_assigned_nonce(4);
        let result = Err::<(), _>(TxManagerError::NonceTooHigh);

        assert!(
            !SimpleTxManager::<RootProvider>::should_reset_nonce_on_send_error(
                &result,
                &send_state,
                None,
            ),
            "resetting would rewind the cache past a concurrent in-flight send nonce",
        );
        assert!(
            SimpleTxManager::<RootProvider>::should_return_reserved_nonce(&result, &send_state),
            "the signed but unpublished nonce must go back to the reuse pool",
        );
    }

    #[rstest]
    #[case::pre_publish_error(false, Err(TxManagerError::Sign("fail".into())), true)]
    #[case::after_publish(true, Err(TxManagerError::Sign("fail".into())), false)]
    #[case::success(false, Ok(()), false)]
    #[case::timeout_no_publish(false, Err(TxManagerError::SendTimeout), true)]
    #[case::timeout_after_publish(true, Err(TxManagerError::SendTimeout), false)]
    #[case::nonce_too_high(false, Err(TxManagerError::NonceTooHigh), true)]
    #[case::nonce_too_low(false, Err(TxManagerError::NonceTooLow), false)]
    fn should_return_reserved_nonce(
        #[case] has_publish: bool,
        #[case] result: crate::TxManagerResult<()>,
        #[case] expected: bool,
    ) {
        let send_state = crate::SendState::new(3).expect("should create send state");
        if has_publish {
            send_state.record_publish();
        }
        assert_eq!(
            SimpleTxManager::<RootProvider>::should_return_reserved_nonce(&result, &send_state,),
            expected,
        );
    }

    #[tokio::test]
    async fn prepare_with_initial_caps_uses_supplied_caps_on_first_attempt() {
        let (manager, _anvil) = setup().await;
        let candidate = TxCandidate {
            to: Some(Address::with_last_byte(0x42)),
            value: U256::from(1_000u64),
            gas_limit: 0,
            ..Default::default()
        };
        let caps = GasPriceCaps {
            gas_tip_cap: 5_000_000_000_000,
            gas_fee_cap: 15_000_000_000_000,
            raw_gas_fee_cap: 15_000_000_000_000,
        };

        let prepared = manager
            .prepare_with_initial_caps(&candidate, None, Some(caps), None, None)
            .await
            .expect("should prepare tx using supplied caps");
        let tx = decode_eip1559(&prepared);

        assert_eq!(tx.to, TxKind::Call(Address::with_last_byte(0x42)));
        assert_eq!(prepared.gas_tip_cap, caps.gas_tip_cap);
        assert_eq!(prepared.gas_fee_cap, caps.gas_fee_cap);
        assert_eq!(tx.max_priority_fee_per_gas, caps.gas_tip_cap);
        assert_eq!(tx.max_fee_per_gas, caps.gas_fee_cap);
        assert!(
            prepared.gas_limit >= 21_000,
            "gas_limit should be >= 21,000 for a value transfer, got {}",
            prepared.gas_limit,
        );
    }
}
