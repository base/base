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
//! [`suggest_gas_price_caps`]: SimpleTxManager::suggest_gas_price_caps
//! [`FeeCalculator::check_limits`]: crate::FeeCalculator::check_limits
//! [`NonceManager`]: crate::NonceManager
//! [`NetworkWallet`]: alloy_network::NetworkWallet
//! [`prepare`]: SimpleTxManager::prepare
//! [`craft_tx`]: SimpleTxManager::craft_tx
//! [`send_tx`]: SimpleTxManager::send_tx
//! [`TransactionRequest`]: alloy_rpc_types_eth::TransactionRequest

use std::{
    fmt::Debug,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_consensus::TxEnvelope;
use alloy_eips::{
    BlockNumberOrTag, Decodable2718, Encodable2718, eip7594::BlobTransactionSidecarEip7594,
};
use alloy_network::{
    Ethereum, EthereumWallet, Network, NetworkTransactionBuilder, NetworkWallet,
    TransactionBuilder, TransactionBuilderError,
};
use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::Provider;
use alloy_rpc_types_eth::{TransactionReceipt, TransactionRequest};
use alloy_transport::TransportError;
use backon::{ConstantBuilder, Retryable};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::{
    BlobTxBuilder, BumpedFees, FeeCalculator, FeeOverride, GasPriceCaps, NonceManager,
    RpcErrorClassifier, SendHandle, SendResponse, SendState, SignerConfig, TxCandidate, TxManager,
    TxManagerConfig, TxManagerError, TxManagerResult, TxMetrics,
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

/// Mutable fee-bump state tracked across iterations in the send loop.
///
/// Groups the values that [`SimpleTxManager::try_fee_bump`] and
/// [`SimpleTxManager::apply_bump_result`] update on each successful
/// fee bump so they can be passed as a single argument.
#[derive(Debug)]
struct BumpState {
    /// Current maximum priority fee per gas (tip).
    tip: u128,
    /// Current maximum total fee per gas (base fee + tip).
    fee_cap: u128,
    /// Current blob fee cap. `None` for non-blob txs.
    blob_fee_cap: Option<u128>,
    /// Current gas limit.
    gas_limit: u64,
    /// Hash of the most recently published transaction.
    tx_hash: B256,
    /// Nonce used in the most recently published transaction.
    nonce: u64,
    /// Cached blob sidecar. See [`PreparedTx::sidecar`] for rationale on
    /// the `Arc` wrapper.
    sidecar: Option<Arc<BlobTransactionSidecarEip7594>>,
}

impl BumpState {
    /// Constructs a [`BumpState`] from a [`PreparedTx`] and the hash of the
    /// published transaction.
    fn from_prepared(prepared: PreparedTx, tx_hash: B256) -> Self {
        Self {
            tip: prepared.gas_tip_cap,
            fee_cap: prepared.gas_fee_cap,
            blob_fee_cap: prepared.blob_fee_cap,
            gas_limit: prepared.gas_limit,
            tx_hash,
            nonce: prepared.nonce,
            sidecar: prepared.sidecar,
        }
    }
}

/// Default transaction manager implementation.
///
/// Constructs, signs, and submits EIP-1559 transactions. All RPC fields
/// (nonce, gas, fees) are set manually on [`TransactionRequest`] without
/// alloy fillers or `PendingTransactionBuilder`.
#[derive(Debug, Clone)]
pub struct SimpleTxManager<P> {
    /// RPC provider for chain queries and transaction submission.
    provider: P,
    /// Wallet used for signing transactions.
    wallet: EthereumWallet,
    /// Validated runtime configuration.
    config: TxManagerConfig,
    /// Nonce manager for sequential nonce allocation.
    nonce_manager: NonceManager<P>,
    /// Chain ID for transaction construction.
    chain_id: u64,
    /// Shutdown flag shared across the manager and any spawned background
    /// tasks. Wrapped in [`Arc`] so that calling [`close`](Self::close)
    /// on the original manager is observed by tasks spawned via
    /// [`send_async`](Self::send_async) and [`wait_for_tx`](Self::wait_for_tx).
    closed: Arc<AtomicBool>,
    /// Metrics collector for transaction lifecycle events.
    metrics: Arc<dyn TxMetrics>,
}

impl<P> SimpleTxManager<P>
where
    P: Provider + Clone + Debug + Send + Sync + 'static,
{
    /// Maximum number of retry attempts for [`Self::prepare`].
    pub const PREPARE_MAX_RETRIES: usize = 30;

    /// Fixed delay between retry attempts in [`Self::prepare`].
    pub const PREPARE_RETRY_DELAY: Duration = Duration::from_secs(2);

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
        config.validate().map_err(|e| TxManagerError::InvalidConfig(e.to_string()))?;

        // Cross-validate chain_id against the provider to catch
        // misconfiguration early rather than failing at tx submission.
        let provider_chain_id =
            tokio::time::timeout(config.network_timeout, provider.get_chain_id())
                .await
                .map_err(|_| TxManagerError::Rpc("get_chain_id timed out".into()))?
                .map_err(|e| RpcErrorClassifier::classify_rpc_error(&e))?;

        if chain_id != provider_chain_id {
            return Err(TxManagerError::InvalidConfig(format!(
                "chain_id mismatch: supplied {chain_id}, provider returned {provider_chain_id}"
            )));
        }

        let address = <EthereumWallet as NetworkWallet<Ethereum>>::default_signer_address(&wallet);
        let nonce_manager = NonceManager::new(provider.clone(), address, config.network_timeout);
        Ok(Self {
            provider,
            wallet,
            config,
            nonce_manager,
            chain_id,
            closed: Arc::new(AtomicBool::new(false)),
            metrics,
        })
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
    pub const fn nonce_manager(&self) -> &NonceManager<P> {
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
    /// Background tasks spawned by [`send_async`](Self::send_async) and
    /// receipt polling tasks will also observe the shutdown and exit
    /// gracefully, since `closed` is shared via [`Arc`].
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    /// Returns `true` if the manager has been closed via [`close`](Self::close).
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
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

    /// Checks both gas and blob fee ceilings relative to the raw provider
    /// estimates in `caps`.
    ///
    /// Calls [`check_fee_limit`](Self::check_fee_limit) for the gas fee cap,
    /// and when `blob_fee_cap` is `Some`, also checks the blob fee cap
    /// against [`raw_blob_baseline`](Self::raw_blob_baseline).
    fn check_fee_limits(
        &self,
        fee_cap: u128,
        blob_fee_cap: Option<u128>,
        caps: &GasPriceCaps,
    ) -> TxManagerResult<()> {
        self.check_fee_limit(fee_cap, caps.raw_gas_fee_cap)?;
        if let Some(blob_cap) = blob_fee_cap {
            self.check_fee_limit(blob_cap, self.raw_blob_baseline(caps)?)?;
        }
        Ok(())
    }

    /// Returns the raw (pre-minimum) blob fee cap from `caps`.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::Unsupported`] if `raw_blob_fee_cap` is `None`,
    /// which indicates a caller bug (blob tx path must always populate this).
    fn raw_blob_baseline(&self, caps: &GasPriceCaps) -> TxManagerResult<u128> {
        caps.raw_blob_fee_cap.ok_or_else(|| {
            TxManagerError::Unsupported("raw_blob_fee_cap missing on blob transaction path".into())
        })
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
    /// satisfy geth's replacement rules.
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

    /// Queries the provider for current gas price estimates.
    ///
    /// Returns a [`GasPriceCaps`] for **non-blob** (EIP-1559) transactions only:
    /// `blob_fee_cap` and `raw_blob_fee_cap` will always be `None`.
    ///
    /// Blob fee estimation is performed internally by
    /// [`craft_tx`](Self::craft_tx) and [`prepare`](Self::prepare) when the
    /// [`TxCandidate`] carries blobs — there is no need to call this method
    /// separately for blob transactions.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::Rpc`] if any provider call fails.
    pub async fn suggest_gas_price_caps(&self) -> TxManagerResult<GasPriceCaps> {
        self.suggest_gas_price_caps_for(false).await
    }

    /// Queries the provider for current gas price estimates.
    ///
    /// When `is_blob` is `true`, also fetches the blob base fee and computes
    /// a blob fee cap via [`FeeCalculator::calc_blob_fee_cap`], enforcing the
    /// configured `min_blob_fee` floor.
    ///
    /// Returns a [`GasPriceCaps`] containing:
    /// - `gas_tip_cap`: maximum priority fee (enforced >= `config.min_tip_cap`)
    /// - `gas_fee_cap`: `tip + 2 * base_fee` via [`FeeCalculator::calc_gas_fee_cap`]
    /// - `raw_gas_fee_cap`: fee cap from the raw provider values before
    ///   enforcing minimums, used as the `suggested` baseline in
    ///   [`FeeCalculator::check_limits`]
    /// - `blob_fee_cap`: `Some(2 × blob_base_fee)` when `is_blob`, else `None`
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::Rpc`] if any provider call fails.
    async fn suggest_gas_price_caps_for(&self, is_blob: bool) -> TxManagerResult<GasPriceCaps> {
        // Query tip cap, latest block, and (optionally) blob base fee concurrently.
        let tip_fut = tokio::time::timeout(
            self.config.network_timeout,
            self.provider.get_max_priority_fee_per_gas(),
        );
        let block_fut = tokio::time::timeout(
            self.config.network_timeout,
            self.provider.get_block_by_number(BlockNumberOrTag::Latest),
        );

        let (tip_result, block_result, blob_fee_result) = if is_blob {
            let blob_fut = tokio::time::timeout(
                self.config.network_timeout,
                self.provider.get_blob_base_fee(),
            );
            let (t, b, bf) = tokio::join!(tip_fut, block_fut, blob_fut);
            (t, b, Some(bf))
        } else {
            let (t, b) = tokio::join!(tip_fut, block_fut);
            (t, b, None)
        };

        let raw_tip_cap = tip_result
            .map_err(|_| self.rpc_error("get_max_priority_fee_per_gas timed out"))?
            .map_err(|e| self.classify_and_record_rpc(&e))?;

        let latest_block = block_result
            .map_err(|_| self.rpc_error("get_block_by_number timed out"))?
            .map_err(|e| self.classify_and_record_rpc(&e))?
            .ok_or_else(|| self.rpc_error("latest block not found"))?;

        let raw_base_fee = u128::from(
            latest_block
                .header
                .base_fee_per_gas
                .ok_or_else(|| self.rpc_error("base fee not available"))?,
        );

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

        // Compute blob fee cap when this is a blob transaction.
        let (blob_fee_cap, raw_blob_fee_cap) = match blob_fee_result {
            Some(result) => {
                let raw_blob_base_fee = result
                    .map_err(|_| self.rpc_error("get_blob_base_fee timed out"))?
                    .map_err(|e| self.classify_and_record_rpc(&e))?;
                let raw_blob_cap = FeeCalculator::calc_blob_fee_cap(raw_blob_base_fee);
                let blob_base_fee = raw_blob_base_fee.max(self.config.min_blob_fee);
                let blob_cap = FeeCalculator::calc_blob_fee_cap(blob_base_fee);
                self.metrics.record_blob_fee(blob_cap as f64 / WEI_PER_GWEI);
                (Some(blob_cap), Some(raw_blob_cap))
            }
            None => (None, None),
        };

        Ok(GasPriceCaps {
            gas_tip_cap: tip_cap,
            gas_fee_cap,
            raw_gas_fee_cap,
            blob_fee_cap,
            raw_blob_fee_cap,
        })
    }

    /// Computes bumped fee parameters for a replacement transaction.
    ///
    /// Given the previous on-wire fees, applies
    /// [`FeeCalculator::update_fees`] to satisfy geth's tx-replacement
    /// rules and enforces the configured fee ceiling via
    /// [`FeeCalculator::check_limits`].
    ///
    /// For blob transactions (`old_blob_fee_cap` is `Some`), the blob fee
    /// cap is bumped separately with a 100 % minimum increase via
    /// [`FeeCalculator::calc_threshold_value`].
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
        candidate: &TxCandidate,
        old_tip: u128,
        old_fee_cap: u128,
        old_blob_fee_cap: Option<u128>,
    ) -> TxManagerResult<BumpedFees> {
        let is_blob = candidate.is_blob();

        // Validate consistency: old_blob_fee_cap must be Some for blob txs
        // and None for non-blob txs.
        if old_blob_fee_cap.is_some() != is_blob {
            return Err(TxManagerError::Unsupported(
                "old_blob_fee_cap must be Some for blob transactions and None for non-blob transactions"
                    .into(),
            ));
        }

        // Step 1: Fetch fresh network fees.
        let caps = self.suggest_gas_price_caps_for(is_blob).await?;

        // Step 2: Derive effective base fee from the caps.
        let new_base_fee = FeeCalculator::base_fee_from_caps(caps.gas_fee_cap, caps.gas_tip_cap);

        // Step 3: Compute bumped tip and fee cap.
        let (bumped_tip, bumped_fee_cap) = FeeCalculator::update_fees(
            old_tip,
            old_fee_cap,
            caps.gas_tip_cap,
            new_base_fee,
            is_blob,
        );

        // Step 4: Bump blob fee cap separately with 100% minimum.
        let blob_fee_cap = old_blob_fee_cap.map(|old_blob| {
            let threshold = FeeCalculator::calc_threshold_value(old_blob, true);
            caps.blob_fee_cap.map_or(threshold, |network_blob| threshold.max(network_blob))
        });

        // Step 5: Enforce fee ceilings on gas and blob fee caps.
        self.check_fee_limits(bumped_fee_cap, blob_fee_cap, &caps)?;

        info!(
            %old_tip,
            %old_fee_cap,
            %bumped_tip,
            %bumped_fee_cap,
            old_blob_fee_cap = ?old_blob_fee_cap,
            blob_fee_cap = ?blob_fee_cap,
            "gas price increase computed",
        );

        Ok(BumpedFees { gas_tip_cap: bumped_tip, gas_fee_cap: bumped_fee_cap, blob_fee_cap, caps })
    }

    /// Constructs and signs a single transaction from a [`TxCandidate`].
    ///
    /// Steps:
    /// 1. Query gas price caps via [`suggest_gas_price_caps`](Self::suggest_gas_price_caps)
    /// 2. Apply fee overrides as a floor (`max(network_fee, override)`)
    /// 3. Check fee limits via [`FeeCalculator::check_limits`]
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

        // Step 1: Get fee estimates (includes blob base fee when is_blob).
        let caps = match caps {
            Some(caps) => caps,
            None => self.suggest_gas_price_caps_for(is_blob).await?,
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

        // Step 3: Compute blob fee cap with config minimum and override floor.
        let blob_fee_cap = if is_blob {
            let network = caps.blob_fee_cap.ok_or_else(|| {
                TxManagerError::Unsupported(
                    "blob_fee_cap missing from fee caps on blob transaction path".into(),
                )
            })?;
            let floor = fee_overrides.as_ref().and_then(|fo| fo.blob_fee_cap).unwrap_or(0);
            Some(network.max(floor))
        } else {
            None
        };

        // Step 3b: Check fee limits.
        //
        // The `suggested` parameter is the raw gas_fee_cap computed from
        // the provider's values before enforcing our configured minimums
        // (`min_tip_cap`, `min_basefee`). This detects when enforced
        // minimums or fee overrides inflate the fee cap beyond
        // `fee_limit_multiplier × raw_provider_fee_cap`.
        self.check_fee_limits(fee_cap, blob_fee_cap, &caps)?;

        // Step 4: Build TransactionRequest.
        let from = self.sender_address();
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
        let estimated = tokio::time::timeout(
            self.config.network_timeout,
            self.provider.estimate_gas(tx_request.clone()),
        )
        .await
        .map_err(|_| self.rpc_error("estimate_gas timed out"))?
        .map_err(|e| {
            let err = self.classify_and_record_rpc(&e);
            if matches!(err, TxManagerError::ExecutionReverted { .. }) {
                error!(error = %err, "gas estimation reverted");
            }
            err
        })?;
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
    /// * **Pre-publish errors** (no tx was ever sent): the nonce is always
    ///   reset so the next attempt re-fetches from the chain.
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
            send_state.set_mempool_deadline(Instant::now() + self.config.tx_not_in_mempool_timeout);
        }

        let start = Instant::now();

        // Wrap the event loop in a send timeout if configured.
        let result = if self.config.tx_send_timeout.is_zero() {
            self.send_event_loop(&candidate, &send_state, nonce_override).await
        } else {
            tokio::time::timeout(
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

        let latency_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.metrics.record_send_latency(latency_ms);

        if result.is_ok() {
            self.metrics.record_tx_confirmed();
        } else {
            self.metrics.record_tx_failed();
        }

        if Self::should_reset_nonce_on_send_error(&result, &send_state, nonce_override) {
            // Three policies (see should_reset_nonce_on_send_error):
            // • NonceTooHigh — always reset, even with nonce_override.
            //   No tx entered the mempool; reserved_high_water protects
            //   concurrent reservations from collision after reset.
            // • SendTimeout — always reset. The timeout may have cancelled
            //   prepare() after it reserved a nonce but before the tx reached
            //   the mempool, leaking the nonce manager's internal counter.
            // • Other errors — reset only when no tx was ever published.
            //   Once a tx may be pending, re-fetching the latest nonce could
            //   conflict with the in-flight transaction.
            self.nonce_manager.reset().await;
        }

        // Return pre-reserved nonces that were never published so they can
        // be reissued by subsequent send/send_async calls, preventing
        // irrecoverable nonce gaps.
        if let Some(n) = nonce_override
            && Self::should_return_reserved_nonce(&result, &send_state)
        {
            self.nonce_manager.return_reserved_nonce(n).await;
        }

        result
    }

    fn should_reset_nonce_on_send_error<T>(
        result: &TxManagerResult<T>,
        send_state: &SendState,
        nonce_override: Option<u64>,
    ) -> bool {
        match result {
            Ok(_) => false,
            // Always reset on NonceTooHigh — the publish failed, no tx
            // entered the mempool, and reserved_high_water protects
            // concurrent reservations from collision after reset.
            Err(TxManagerError::NonceTooHigh) => true,
            // When a nonce_override was used, the nonce was irrevocably
            // consumed at reserve_nonce() time. Resetting the nonce manager
            // would corrupt concurrent send_async reservations.
            _ if nonce_override.is_some() => false,
            // Always reset on timeout — the timeout may have cancelled
            // prepare() mid-flight during a fee bump, leaking a nonce
            // from the nonce manager's internal counter.
            Err(TxManagerError::SendTimeout) => true,
            // For other errors, reset only if nothing was ever published.
            // Once a transaction is pending, the next send_tx will re-sync
            // via the chain's pending nonce anyway.
            Err(_) => !send_state.has_published(),
        }
    }

    /// Returns `true` when a pre-reserved nonce should be returned to the
    /// nonce manager's reuse pool.
    ///
    /// A nonce is eligible for return when BOTH of these hold:
    /// 1. The send failed (`result.is_err()`).
    /// 2. No transaction was ever successfully published — if a tx was
    ///    published, the nonce may be in the mempool and must not be
    ///    reused.
    ///
    /// The caller is responsible for checking that a nonce override exists
    /// (i.e. the send came from `send_async`, not `send`).
    fn should_return_reserved_nonce<T>(
        result: &TxManagerResult<T>,
        send_state: &SendState,
    ) -> bool {
        match result {
            // Nonce errors indicate the nonce is genuinely invalid, not a
            // transient failure.  Returning it to the reuse pool would
            // cause an infinite retry loop: after a reset() the chain
            // nonce is re-fetched, but advance_nonce() pops
            // returned_nonces first, reissuing the same invalid value.
            Ok(_) | Err(TxManagerError::NonceTooHigh | TxManagerError::NonceTooLow) => false,
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
    async fn send_event_loop(
        &self,
        candidate: &TxCandidate,
        send_state: &Arc<SendState>,
        nonce_override: Option<u64>,
    ) -> SendResponse {
        // Initial transaction preparation. prepare() is NOT cancellation-safe,
        // so it runs to completion before entering the select loop.
        // The returned PreparedTx carries the actual on-wire fees, eliminating
        // the need for a separate suggest_gas_price_caps() call.
        let prepared =
            self.prepare_with_initial_caps(candidate, None, None, nonce_override, None).await?;
        let tx_hash = self.publish_tx(send_state, &prepared.raw_tx, None).await?;
        let mut bump = BumpState::from_prepared(prepared, tx_hash);

        // Receipt delivery channel — mpsc because fee bumps may spawn
        // new wait tasks with different tx hashes.
        let (receipt_tx, mut receipt_rx) = mpsc::channel::<TransactionReceipt>(1);

        // Spawn background receipt polling for the initial tx hash.
        Self::wait_for_tx(
            Arc::clone(send_state),
            self.provider.clone(),
            bump.tx_hash,
            self.config.clone(),
            receipt_tx.clone(),
            Arc::clone(&self.closed),
        );

        // Resubmission timer for fee bumping.
        let mut bump_ticker = tokio::time::interval(self.config.resubmission_timeout);
        // Consume the first immediate tick.
        bump_ticker.tick().await;

        loop {
            // Check shutdown and critical errors before blocking.
            if self.is_closed() {
                return Err(TxManagerError::ChannelClosed);
            }
            if let Some(err) = send_state.critical_error() {
                error!(error = %err, "critical error, aborting send");
                return Err(err);
            }

            // If a receipt is already waiting, return it immediately
            // instead of performing a wasted fee bump.
            if let Ok(receipt) = receipt_rx.try_recv() {
                info!(
                    tx_hash = %receipt.transaction_hash,
                    block = ?receipt.block_number,
                    status = %receipt.inner.status(),
                    "transaction receipt received",
                );
                return Ok(receipt);
            }

            // Respond immediately to the bump-fees flag set by
            // process_send_error on retryable errors, rather than waiting
            // for the next resubmission timer tick. take_bump_fees clears
            // the flag atomically so a failed bump does not re-trigger.
            if send_state.take_bump_fees() {
                if let Some(abort) =
                    self.try_fee_bump(candidate, send_state, &receipt_tx, &mut bump).await
                {
                    return Err(abort);
                }
                // Reset the bump ticker so we get a full interval
                // before the next timer-driven bump.
                bump_ticker.reset();
                continue;
            }

            // Determine which event fired. handle_fee_bump (and
            // transitively prepare()) is NOT cancellation-safe, so it
            // must not run inside tokio::select!. The select block only
            // captures which arm won; fee bump work runs to completion
            // outside the select block.
            tokio::select! {
                biased;
                result = receipt_rx.recv() => {
                    match result {
                        Some(receipt) => {
                            info!(
                                tx_hash = %receipt.transaction_hash,
                                block = ?receipt.block_number,
                                status = %receipt.inner.status(),
                                "transaction receipt received",
                            );
                            return Ok(receipt);
                        }
                        None => {
                            // All senders dropped — should not happen in normal flow.
                            return Err(TxManagerError::ChannelClosed);
                        }
                    }
                }
                _ = bump_ticker.tick() => {}
            };

            // Bump tick fired — run fee bump logic.
            if let Some(abort) =
                self.try_fee_bump(candidate, send_state, &receipt_tx, &mut bump).await
            {
                return Err(abort);
            }
            bump_ticker.reset();
        }
    }

    /// Performs a fee bump attempt and applies the result to the tracked state.
    ///
    /// Returns `Some(error)` if the send loop must abort, `None` to continue.
    ///
    /// When a bump attempt yields a non-retryable error but the
    /// receipt-polling task has already recorded a mined tx on
    /// `send_state`, the abort is suppressed so the send loop can
    /// deliver the receipt on the next iteration. This avoids a
    /// false-negative failure report when, for example, a fee bump
    /// publish gets `NonceTooLow` because the original tx was already
    /// confirmed.
    async fn try_fee_bump(
        &self,
        candidate: &TxCandidate,
        send_state: &Arc<SendState>,
        receipt_tx: &mpsc::Sender<TransactionReceipt>,
        state: &mut BumpState,
    ) -> Option<TxManagerError> {
        // If the receipt-polling task has already detected the original tx
        // on-chain, skip the fee bump entirely. This avoids a race where
        // `estimate_gas` on the replacement tx reverts because the
        // original calldata has already executed (e.g. `GameAlreadyExists`),
        // producing noisy error logs even though the proposal will succeed.
        if send_state.is_waiting_for_confirmation() {
            info!("skipping fee bump — original tx already mined, awaiting confirmation");
            return None;
        }

        let result = self.handle_fee_bump(candidate, send_state, receipt_tx, state).await;
        Self::apply_bump_result_with_suppression(result, state, send_state)
    }

    /// Applies a fee bump result with mined-tx suppression.
    ///
    /// If the bump failed with a non-retryable error *but* the original
    /// tx was already mined ([`SendState::is_waiting_for_confirmation`]),
    /// the error is suppressed and logged at `info` level (not `error`).
    /// This prevents false-positive alerts in the benign "tx confirmed
    /// while we were bumping" case.
    ///
    /// On success or retryable error, delegates directly to
    /// [`apply_bump_result`](Self::apply_bump_result).
    fn apply_bump_result_with_suppression(
        result: TxManagerResult<BumpState>,
        state: &mut BumpState,
        send_state: &SendState,
    ) -> Option<TxManagerError> {
        // Check for mined-tx suppression *before* apply_bump_result so
        // the `error!` log inside that function is never emitted for the
        // benign "tx already confirmed" case.
        if let Err(ref e) = result
            && !e.is_retryable()
            && send_state.is_waiting_for_confirmation()
        {
            info!(
                error = %e,
                "fee bump failed but original tx already mined, suppressing abort"
            );
            return None;
        }
        Self::apply_bump_result(result, state)
    }

    /// Handles a single fee bump iteration.
    ///
    /// Delegates fee computation to [`increase_gas_price`](Self::increase_gas_price),
    /// rebuilds the transaction reusing the same nonce via `nonce_override`,
    /// re-publishes it, and spawns a new receipt polling task.
    ///
    /// The bumped fee values are passed as fee overrides to
    /// [`prepare_with_initial_caps`](Self::prepare_with_initial_caps),
    /// which forwards them to [`craft_tx_with_caps`](Self::craft_tx_with_caps).
    /// There, each override is used as a floor via `max(network_fee, override)`,
    /// guaranteeing the replacement transaction meets geth's replacement
    /// thresholds even if network fees have dropped since the bump was
    /// calculated.
    ///
    /// Returns the updated [`BumpState`] on success. The fee values are
    /// taken directly from the [`PreparedTx`] returned by `prepare()`,
    /// reflecting the *actual* on-wire fees (which are guaranteed >= the
    /// bumped values) so that subsequent bump iterations use an accurate
    /// baseline.
    async fn handle_fee_bump(
        &self,
        candidate: &TxCandidate,
        send_state: &Arc<SendState>,
        receipt_tx: &mpsc::Sender<TransactionReceipt>,
        old: &BumpState,
    ) -> TxManagerResult<BumpState> {
        let bumped =
            self.increase_gas_price(candidate, old.tip, old.fee_cap, old.blob_fee_cap).await?;

        let fee_override = bumped.to_fee_override(old.gas_limit);

        // Rebuild transaction with bumped fees as overrides and the fresh
        // caps to avoid a redundant provider round-trip.
        // Reuse the exact nonce from the transaction being replaced via
        // nonce_override, rather than resetting the nonce manager.
        // Resetting would corrupt nonces pre-reserved by concurrent
        // send_async() tasks.
        let prepared = self
            .prepare_with_initial_caps(
                candidate,
                Some(fee_override),
                Some(bumped.caps),
                Some(old.nonce),
                old.sidecar.clone(),
            )
            .await?;

        let new_hash = self.publish_tx(send_state, &prepared.raw_tx, Some(old.tx_hash)).await?;

        // Record the bump and log only after the transaction has been
        // successfully published to avoid inflating the count on failure.
        let bump_count = send_state.record_fee_bump();
        self.metrics.record_gas_bump();
        info!(
            bump_count = %bump_count,
            old_tip = %old.tip,
            new_tip = %prepared.gas_tip_cap,
            old_fee_cap = %old.fee_cap,
            new_fee_cap = %prepared.gas_fee_cap,
            old_blob_fee_cap = ?old.blob_fee_cap,
            new_blob_fee_cap = ?prepared.blob_fee_cap,
            old_gas_limit = %old.gas_limit,
            new_gas_limit = %prepared.gas_limit,
            "fee bump applied",
        );

        // Spawn a new receipt polling task for the bumped tx.
        Self::wait_for_tx(
            Arc::clone(send_state),
            self.provider.clone(),
            new_hash,
            self.config.clone(),
            receipt_tx.clone(),
            Arc::clone(&self.closed),
        );

        Ok(BumpState::from_prepared(prepared, new_hash))
    }

    /// Applies the result of a fee bump attempt, updating the tracked fee
    /// state on success or logging the error on failure.
    ///
    /// Returns `Some(error)` when the caller must abort the send loop
    /// (non-retryable error), or `None` when the loop should continue
    /// (success or retryable error).
    fn apply_bump_result(
        result: TxManagerResult<BumpState>,
        state: &mut BumpState,
    ) -> Option<TxManagerError> {
        match result {
            Ok(new_state) => {
                *state = new_state;
                None
            }
            Err(e) if !e.is_retryable() => {
                error!(error = %e, "non-retryable error during fee bump");
                Some(e)
            }
            Err(e) => {
                warn!(error = %e, "fee bump failed, will retry next tick");
                None
            }
        }
    }

    /// Broadcasts a raw transaction to the network.
    ///
    /// On success, records a successful publish on the [`SendState`] and
    /// returns the transaction hash. On [`TxManagerError::AlreadyKnown`]
    /// after a prior successful publish, treats it as success — records
    /// another successful publish and returns `Ok(last_tx_hash)` so
    /// callers do not need to special-case this variant. All other errors
    /// are forwarded to [`SendState::process_send_error`] for state tracking.
    ///
    /// The `last_tx_hash` parameter is the hash from the most recent
    /// successful publish. When the network returns `AlreadyKnown` on
    /// resubmission, this hash is returned as the successful result.
    ///
    /// # Errors
    ///
    /// Returns the classified error on publication failure.
    pub async fn publish_tx(
        &self,
        send_state: &SendState,
        raw_tx: &Bytes,
        last_tx_hash: Option<B256>,
    ) -> TxManagerResult<B256> {
        let result = tokio::time::timeout(
            self.config.network_timeout,
            self.provider.send_raw_transaction(raw_tx),
        )
        .await;

        match result {
            Ok(Ok(pending)) => {
                let tx_hash = *pending.tx_hash();
                send_state.record_successful_publish();
                info!(tx_hash = %tx_hash, "transaction published");
                Ok(tx_hash)
            }
            Ok(Err(e)) => {
                let classified = self.classify_and_record_rpc(&e);

                // AlreadyKnown on resubmission is a success — the tx is in
                // the mempool from a prior publish. Return the previous hash.
                if classified.is_already_known() && send_state.has_published() {
                    let hash = last_tx_hash.ok_or_else(|| {
                        TxManagerError::InvalidConfig(
                            "AlreadyKnown but no prior tx hash available — caller must track the hash from publish_tx".into(),
                        )
                    })?;
                    send_state.record_successful_publish();
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
                } else {
                    error!(error = %classified, "publish failed with non-retryable error");
                }

                Err(classified)
            }
            Err(_) => {
                let err = self.rpc_error("send_raw_transaction timed out");
                send_state.process_send_error(&err);
                self.metrics.record_publish_error();
                warn!(error = %err, "publish timed out");
                Err(err)
            }
        }
    }

    /// Spawns a background task that polls for a transaction receipt and
    /// sends it through the provided channel once confirmed.
    ///
    /// The task delegates to [`wait_mined`](Self::wait_mined), which polls
    /// internally until the transaction is confirmed or the manager shuts
    /// down.
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
    ///   channel (or finds the channel closed because a different poller
    ///   already delivered).
    /// - Polling frequency is governed by `receipt_query_interval`, so
    ///   RPC load is proportional to `bump_count × 1/interval`.
    pub fn wait_for_tx(
        send_state: Arc<SendState>,
        provider: P,
        tx_hash: B256,
        config: TxManagerConfig,
        receipt_tx: mpsc::Sender<TransactionReceipt>,
        closed: Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            debug!(tx_hash = %tx_hash, "starting receipt polling");

            let receipt = Self::wait_mined(&send_state, &provider, tx_hash, &config, &closed).await;
            if let Some(receipt) = receipt {
                // Best-effort send — if the receiver is dropped, the
                // send loop has already exited (e.g., another tx confirmed).
                let _ = receipt_tx.send(receipt).await;
            }

            debug!(tx_hash = %tx_hash, "receipt polling ended");
        })
    }

    /// Polls for a transaction receipt at `receipt_query_interval` until
    /// the transaction is mined and confirmed to the required depth.
    ///
    /// Returns `Some(receipt)` when the transaction reaches
    /// `num_confirmations` depth, or `None` if the manager is closed or
    /// the `confirmation_timeout` deadline is exceeded.
    pub async fn wait_mined(
        send_state: &SendState,
        provider: &P,
        tx_hash: B256,
        config: &TxManagerConfig,
        closed: &AtomicBool,
    ) -> Option<TransactionReceipt> {
        let deadline = Instant::now() + config.confirmation_timeout;
        let mut poll_interval = tokio::time::interval(config.receipt_query_interval);
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            poll_interval.tick().await;

            match Self::query_receipt(
                send_state,
                provider,
                tx_hash,
                config.num_confirmations,
                config.network_timeout,
            )
            .await
            {
                Ok(Some(receipt)) => return Some(receipt),
                Ok(None) => {
                    // Not yet confirmed — continue polling.
                }
                Err(e) => {
                    warn!(tx_hash = %tx_hash, error = %e, "receipt query failed");
                }
            }

            if Instant::now() >= deadline {
                warn!(
                    tx_hash = %tx_hash,
                    timeout = ?config.confirmation_timeout,
                    "confirmation timeout exceeded",
                );
                return None;
            }

            // Check shutdown state each iteration to support cancellation.
            if closed.load(Ordering::Acquire) {
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
        // Fetch tip and receipt in parallel. The canonical block hash check
        // below ensures the receipt is on the canonical chain, so call order
        // no longer matters for reorg safety.
        let (tip_result, receipt_result) = tokio::join!(
            tokio::time::timeout(network_timeout, provider.get_block_number()),
            tokio::time::timeout(network_timeout, provider.get_transaction_receipt(tx_hash)),
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

        let canonical_block = tokio::time::timeout(
            network_timeout,
            provider.get_block_by_number(BlockNumberOrTag::Number(tx_block)),
        )
        .await
        .map_err(|_| TxManagerError::Rpc("get_block_by_number timed out".into()))?
        .map_err(|e| RpcErrorClassifier::classify_rpc_error(&e))?;

        let is_canonical =
            canonical_block.as_ref().is_some_and(|block| block.header.hash == receipt_block_hash);

        if !is_canonical {
            send_state.tx_not_mined(tx_hash);
            debug!(
                tx_hash = %tx_hash,
                tx_block = %tx_block,
                receipt_block_hash = %receipt_block_hash,
                "receipt block hash does not match canonical chain",
            );
            return Ok(None);
        }

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
            Ok(Some(receipt))
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

impl<P> TxManager for SimpleTxManager<P>
where
    P: Provider + Clone + Debug + Send + Sync + 'static,
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

        tokio::spawn(async move {
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
    use std::sync::Arc;

    use alloy_consensus::TxEip1559;
    use alloy_network::EthereumWallet;
    use alloy_node_bindings::Anvil;
    use alloy_primitives::{Address, B256, TxKind, U256};
    use alloy_provider::RootProvider;
    use alloy_signer_local::PrivateKeySigner;
    use rstest::rstest;

    use super::{BumpState, PreparedTx, SimpleTxManager, TxEnvelope};
    use crate::{
        GasPriceCaps, NoopTxMetrics, SendState, TxCandidate, TxManagerConfig, TxManagerError,
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

    fn decode_eip1559(prepared: &PreparedTx) -> TxEip1559 {
        match prepared.to_envelope().expect("should decode as valid TxEnvelope") {
            TxEnvelope::Eip1559(signed) => signed.strip_signature(),
            other => panic!("expected EIP-1559, got {other:?}"),
        }
    }

    // ── apply_bump_result ─────────────────────────────────────────────

    #[rstest]
    #[case::success_updates_state(
        Ok(BumpState { tip: 200, fee_cap: 2000, blob_fee_cap: None, gas_limit: 50_000, tx_hash: B256::with_last_byte(0x42), nonce: 0, sidecar: None }),
        false, 200, 2000, None, 50_000, B256::with_last_byte(0x42),
    )]
    #[case::success_with_blob_fee_cap(
        Ok(BumpState { tip: 300, fee_cap: 3000, blob_fee_cap: Some(10_000), gas_limit: 60_000, tx_hash: B256::with_last_byte(0x43), nonce: 1, sidecar: None }),
        false, 300, 3000, Some(10_000), 60_000, B256::with_last_byte(0x43),
    )]
    #[case::non_retryable_returns_abort(
        Err(TxManagerError::FeeLimitExceeded { fee: 500, ceiling: 100 }),
        true, 100, 1000, Some(5_000), 21_000, B256::ZERO,
    )]
    #[case::retryable_continues(
        Err(TxManagerError::Rpc("transient error".to_string())),
        false, 100, 1000, Some(5_000), 21_000, B256::ZERO,
    )]
    fn apply_bump_result(
        #[case] input: Result<BumpState, TxManagerError>,
        #[case] abort_expected: bool,
        #[case] expected_tip: u128,
        #[case] expected_fee_cap: u128,
        #[case] expected_blob_fee_cap: Option<u128>,
        #[case] expected_gas_limit: u64,
        #[case] expected_hash: B256,
    ) {
        let mut state = BumpState {
            tip: 100,
            fee_cap: 1000,
            blob_fee_cap: Some(5_000),
            gas_limit: 21_000,
            tx_hash: B256::ZERO,
            nonce: 0,
            sidecar: None,
        };

        let abort = SimpleTxManager::<RootProvider>::apply_bump_result(input, &mut state);

        assert_eq!(abort.is_some(), abort_expected);
        assert_eq!(state.tip, expected_tip);
        assert_eq!(state.fee_cap, expected_fee_cap);
        assert_eq!(state.blob_fee_cap, expected_blob_fee_cap);
        assert_eq!(state.gas_limit, expected_gas_limit);
        assert_eq!(state.tx_hash, expected_hash);
    }

    // ── apply_bump_result_with_suppression ───────────────────────────
    //
    // Tests for `apply_bump_result_with_suppression`, which is the
    // extracted helper that `try_fee_bump` calls. When a non-retryable
    // error occurs but the original tx is already mined, the abort is
    // suppressed so the send loop can deliver the receipt.

    /// Threshold used for `SendState` construction in suppression tests.
    const SUPPRESSION_NONCE_TOO_LOW_THRESHOLD: u64 = 3;

    /// Hash recorded as a mined tx when simulating a confirmed original.
    const MINED_TX_HASH: B256 = B256::repeat_byte(0xAA);

    /// Default initial `BumpState` for suppression tests.
    fn default_bump_state() -> BumpState {
        BumpState {
            tip: 100,
            fee_cap: 1000,
            blob_fee_cap: None,
            gas_limit: 21_000,
            tx_hash: B256::ZERO,
            nonce: 0,
            sidecar: None,
        }
    }

    /// Creates a `SendState` and optionally marks a tx as mined.
    fn suppression_send_state(tx_mined: bool) -> SendState {
        let state =
            SendState::new(SUPPRESSION_NONCE_TOO_LOW_THRESHOLD).expect("should create send state");
        if tx_mined {
            state.tx_mined(MINED_TX_HASH);
        }
        state
    }

    /// Non-retryable errors are suppressed when the original tx is
    /// mined, and propagated when it is not.
    #[rstest]
    #[case::nonce_too_low_mined_suppressed(
        TxManagerError::NonceTooLow,
        true,  // tx is mined
        false, // abort suppressed → no abort
    )]
    #[case::nonce_too_low_not_mined_propagates(
        TxManagerError::NonceTooLow,
        false, // tx is NOT mined
        true,  // abort propagates
    )]
    #[case::fee_limit_exceeded_mined_suppressed(
        TxManagerError::FeeLimitExceeded { fee: 500, ceiling: 100 },
        true,
        false,
    )]
    #[case::fee_limit_exceeded_not_mined_propagates(
        TxManagerError::FeeLimitExceeded { fee: 500, ceiling: 100 },
        false,
        true,
    )]
    #[case::insufficient_funds_mined_suppressed(TxManagerError::InsufficientFunds, true, false)]
    #[case::insufficient_funds_not_mined_propagates(TxManagerError::InsufficientFunds, false, true)]
    #[case::execution_reverted_mined_suppressed(
        TxManagerError::ExecutionReverted { reason: Some("revert".into()), data: None },
        true,
        false,
    )]
    #[case::execution_reverted_not_mined_propagates(
        TxManagerError::ExecutionReverted { reason: Some("revert".into()), data: None },
        false,
        true,
    )]
    #[case::channel_closed_mined_suppressed(TxManagerError::ChannelClosed, true, false)]
    #[case::channel_closed_not_mined_propagates(TxManagerError::ChannelClosed, false, true)]
    fn bump_result_with_suppression_non_retryable(
        #[case] error: TxManagerError,
        #[case] tx_mined: bool,
        #[case] abort_expected: bool,
    ) {
        let send_state = suppression_send_state(tx_mined);
        assert_eq!(send_state.is_waiting_for_confirmation(), tx_mined);

        let mut state = default_bump_state();

        let abort = SimpleTxManager::<RootProvider>::apply_bump_result_with_suppression(
            Err(error),
            &mut state,
            &send_state,
        );

        assert_eq!(
            abort.is_some(),
            abort_expected,
            "abort_expected={abort_expected}, tx_mined={tx_mined}",
        );
    }

    /// Retryable errors are never aborted by `apply_bump_result`, so
    /// suppression is irrelevant — the send loop continues regardless.
    #[rstest]
    #[case::rpc_error_mined(TxManagerError::Rpc("transient".into()), true)]
    #[case::rpc_error_not_mined(TxManagerError::Rpc("transient".into()), false)]
    #[case::underpriced_mined(TxManagerError::Underpriced, true)]
    #[case::underpriced_not_mined(TxManagerError::Underpriced, false)]
    #[case::replacement_underpriced_mined(TxManagerError::ReplacementUnderpriced, true)]
    #[case::fee_too_low_not_mined(TxManagerError::FeeTooLow, false)]
    fn bump_result_with_suppression_retryable_never_aborts(
        #[case] error: TxManagerError,
        #[case] tx_mined: bool,
    ) {
        let send_state = suppression_send_state(tx_mined);
        let mut state = default_bump_state();

        let abort = SimpleTxManager::<RootProvider>::apply_bump_result_with_suppression(
            Err(error),
            &mut state,
            &send_state,
        );

        assert!(abort.is_none(), "retryable errors should never cause an abort");
    }

    /// Success results pass through to `apply_bump_result` and update
    /// state, regardless of mined state.
    #[rstest]
    #[case::success_not_mined(false)]
    #[case::success_mined(true)]
    fn bump_result_with_suppression_success_never_aborts(#[case] tx_mined: bool) {
        let send_state = suppression_send_state(tx_mined);

        let new_state = BumpState {
            tip: 200,
            fee_cap: 2000,
            blob_fee_cap: None,
            gas_limit: 50_000,
            tx_hash: B256::with_last_byte(0x42),
            nonce: 0,
            sidecar: None,
        };

        let mut state = default_bump_state();

        let abort = SimpleTxManager::<RootProvider>::apply_bump_result_with_suppression(
            Ok(new_state),
            &mut state,
            &send_state,
        );

        assert!(abort.is_none(), "success should never cause an abort");
        // State should be updated to the new values.
        assert_eq!(state.tip, 200);
        assert_eq!(state.fee_cap, 2000);
        assert_eq!(state.tx_hash, B256::with_last_byte(0x42));
    }

    // ── should_reset_nonce_on_send_error ───────────────────────────────

    #[rstest]
    #[case::error_before_first_publish(false, Err(TxManagerError::SendTimeout), None, true)]
    #[case::non_timeout_error_after_publish(true, Err(TxManagerError::ChannelClosed), None, false)]
    #[case::timeout_after_publish(true, Err(TxManagerError::SendTimeout), None, true)]
    #[case::success(false, Ok(()), None, false)]
    #[case::nonce_too_high_no_override(false, Err(TxManagerError::NonceTooHigh), None, true)]
    #[case::nonce_too_high_with_override(false, Err(TxManagerError::NonceTooHigh), Some(42), true)]
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
            send_state.record_successful_publish();
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

    #[rstest]
    #[case::pre_publish_error(false, Err(TxManagerError::Sign("fail".into())), true)]
    #[case::after_publish(true, Err(TxManagerError::Sign("fail".into())), false)]
    #[case::success(false, Ok(()), false)]
    #[case::timeout_no_publish(false, Err(TxManagerError::SendTimeout), true)]
    #[case::timeout_after_publish(true, Err(TxManagerError::SendTimeout), false)]
    #[case::nonce_too_high(false, Err(TxManagerError::NonceTooHigh), false)]
    #[case::nonce_too_low(false, Err(TxManagerError::NonceTooLow), false)]
    fn should_return_reserved_nonce(
        #[case] has_publish: bool,
        #[case] result: crate::TxManagerResult<()>,
        #[case] expected: bool,
    ) {
        let send_state = crate::SendState::new(3).expect("should create send state");
        if has_publish {
            send_state.record_successful_publish();
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
            blob_fee_cap: None,
            raw_blob_fee_cap: None,
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
