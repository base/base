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
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_eips::{BlockNumberOrTag, Encodable2718};
use alloy_network::{Ethereum, EthereumWallet, NetworkWallet, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::{TransactionReceipt, TransactionRequest};
use backon::{ConstantBuilder, Retryable};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::{
    FeeCalculator, FeeOverride, GasPriceCaps, NonceManager, RpcErrorClassifier, SendHandle,
    SendResponse, SendState, TxCandidate, TxManager, TxManagerConfig, TxManagerError,
    TxManagerResult,
};

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
}

/// Default transaction manager implementation.
///
/// Constructs, signs, and submits EIP-1559 transactions. All RPC fields
/// (nonce, gas, fees) are set manually on [`TransactionRequest`] without
/// alloy fillers or `PendingTransactionBuilder`.
#[derive(Debug, Clone)]
pub struct SimpleTxManager {
    /// RPC provider for chain queries and transaction submission.
    provider: RootProvider,
    /// Wallet used for signing transactions.
    wallet: EthereumWallet,
    /// Validated runtime configuration.
    config: TxManagerConfig,
    /// Nonce manager for sequential nonce allocation.
    nonce_manager: NonceManager,
    /// Chain ID for transaction construction.
    chain_id: u64,
    /// Shutdown flag shared across the manager and any spawned background
    /// tasks. Wrapped in [`Arc`] so that calling [`close`](Self::close)
    /// on the original manager is observed by tasks spawned via
    /// [`send_async`](Self::send_async) and [`wait_for_tx`](Self::wait_for_tx).
    closed: Arc<AtomicBool>,
}

impl SimpleTxManager {
    /// Maximum number of retry attempts for [`Self::prepare`].
    pub const PREPARE_MAX_RETRIES: usize = 30;

    /// Fixed delay between retry attempts in [`Self::prepare`].
    pub const PREPARE_RETRY_DELAY: Duration = Duration::from_secs(2);

    /// Creates a new [`SimpleTxManager`].
    ///
    /// Internally creates a [`NonceManager`] using the wallet's default
    /// signer address and the config's `network_timeout`. Fetches the
    /// chain ID from the provider and validates it against the supplied
    /// `chain_id` to prevent constructing transactions with the wrong
    /// chain ID.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::InvalidConfig`] if config validation
    /// fails or the chain ID does not match the provider. Returns
    /// [`TxManagerError::Rpc`] if the provider is unreachable.
    pub async fn new(
        provider: RootProvider,
        wallet: EthereumWallet,
        config: TxManagerConfig,
        chain_id: u64,
    ) -> TxManagerResult<Self> {
        config.validate().map_err(|e| TxManagerError::InvalidConfig(e.to_string()))?;

        // Cross-validate chain_id against the provider to catch
        // misconfiguration early rather than failing at tx submission.
        let provider_chain_id =
            tokio::time::timeout(config.network_timeout, provider.get_chain_id())
                .await
                .map_err(|_| TxManagerError::Rpc("get_chain_id timed out".into()))?
                .map_err(|e| RpcErrorClassifier::classify_rpc_error(&e.to_string()))?;

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
        })
    }

    /// Returns a reference to the RPC provider.
    pub const fn provider(&self) -> &RootProvider {
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
    pub const fn nonce_manager(&self) -> &NonceManager {
        &self.nonce_manager
    }

    /// Returns the chain ID.
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
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
        self.prepare_with_initial_caps(candidate, fee_overrides, None).await
    }

    /// Internal variant of [`prepare`](Self::prepare) that optionally reuses
    /// caller-supplied fee caps on the first attempt only.
    ///
    /// This is used by the fee-bump path to avoid a redundant
    /// `suggest_gas_price_caps()` round-trip after it has already fetched caps
    /// to compute bump thresholds. Retries intentionally fall back to fresh fee
    /// estimation so transient failures do not pin stale network conditions.
    async fn prepare_with_initial_caps(
        &self,
        candidate: &TxCandidate,
        fee_overrides: Option<FeeOverride>,
        initial_caps: Option<GasPriceCaps>,
    ) -> TxManagerResult<PreparedTx> {
        let mut initial_caps = initial_caps;
        (|| {
            let caps = initial_caps.take();
            async move {
                // Re-check closed flag on each retry attempt to avoid wasted
                // RPC calls after shutdown. ChannelClosed is non-retryable,
                // so backon exits the loop immediately.
                if self.is_closed() {
                    return Err(TxManagerError::ChannelClosed);
                }
                self.craft_tx_with_caps(candidate, fee_overrides, caps).await
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
    /// Returns a [`GasPriceCaps`] containing:
    /// - `gas_tip_cap`: maximum priority fee (enforced >= `config.min_tip_cap`)
    /// - `gas_fee_cap`: `tip + 2 * base_fee` via [`FeeCalculator::calc_gas_fee_cap`]
    /// - `raw_gas_fee_cap`: fee cap from the raw provider values before
    ///   enforcing minimums, used as the `suggested` baseline in
    ///   [`FeeCalculator::check_limits`]
    /// - `blob_fee_cap`: `None` (blob transactions not yet supported)
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::Rpc`] if any provider call fails.
    pub async fn suggest_gas_price_caps(&self) -> TxManagerResult<GasPriceCaps> {
        // Query tip cap and latest block concurrently.
        let (tip_result, block_result) = tokio::join!(
            tokio::time::timeout(
                self.config.network_timeout,
                self.provider.get_max_priority_fee_per_gas(),
            ),
            tokio::time::timeout(
                self.config.network_timeout,
                self.provider.get_block_by_number(BlockNumberOrTag::Latest),
            ),
        );

        let raw_tip_cap = tip_result
            .map_err(|_| TxManagerError::Rpc("get_max_priority_fee_per_gas timed out".into()))?
            .map_err(|e| RpcErrorClassifier::classify_rpc_error(&e.to_string()))?;

        let latest_block = block_result
            .map_err(|_| TxManagerError::Rpc("get_block_by_number timed out".into()))?
            .map_err(|e| RpcErrorClassifier::classify_rpc_error(&e.to_string()))?
            .ok_or_else(|| TxManagerError::Rpc("latest block not found".to_string()))?;

        let raw_base_fee = u128::from(
            latest_block
                .header
                .base_fee_per_gas
                .ok_or_else(|| TxManagerError::Rpc("base fee not available".to_string()))?,
        );

        // Compute raw gas fee cap from provider values before enforcing minimums.
        let raw_gas_fee_cap = FeeCalculator::calc_gas_fee_cap(raw_base_fee, raw_tip_cap);

        // Enforce minimum tip cap and base fee.
        let tip_cap = raw_tip_cap.max(self.config.min_tip_cap);
        let base_fee = raw_base_fee.max(self.config.min_basefee);

        // Compute gas fee cap with enforced minimums.
        let gas_fee_cap = FeeCalculator::calc_gas_fee_cap(base_fee, tip_cap);

        Ok(GasPriceCaps { gas_tip_cap: tip_cap, gas_fee_cap, raw_gas_fee_cap, blob_fee_cap: None })
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
        self.craft_tx_with_caps(candidate, fee_overrides, None).await
    }

    /// Internal variant of [`craft_tx`](Self::craft_tx) that optionally uses
    /// pre-fetched fee caps instead of querying the provider again.
    async fn craft_tx_with_caps(
        &self,
        candidate: &TxCandidate,
        fee_overrides: Option<FeeOverride>,
        caps: Option<GasPriceCaps>,
    ) -> TxManagerResult<PreparedTx> {
        // Blob transactions are not yet supported.
        if !candidate.blobs.is_empty() {
            return Err(TxManagerError::Unsupported(
                "blob transactions are not yet supported".to_string(),
            ));
        }

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
        let (tip_cap, fee_cap) = match fee_overrides {
            Some(FeeOverride { gas_tip_cap, gas_fee_cap }) => {
                (caps.gas_tip_cap.max(gas_tip_cap), caps.gas_fee_cap.max(gas_fee_cap))
            }
            None => (caps.gas_tip_cap, caps.gas_fee_cap),
        };

        // Step 3: Check fee limits.
        //
        // The `suggested` parameter is the raw gas_fee_cap computed from
        // the provider's values before enforcing our configured minimums
        // (`min_tip_cap`, `min_basefee`). This detects when enforced
        // minimums or fee overrides inflate the fee cap beyond
        // `fee_limit_multiplier × raw_provider_fee_cap`.
        FeeCalculator::check_limits(
            fee_cap,
            caps.raw_gas_fee_cap,
            self.config.fee_limit_multiplier,
            self.config.fee_limit_threshold,
        )?;

        // Step 4: Build TransactionRequest.
        let from = self.sender_address();
        let mut tx_request = TransactionRequest::default()
            .with_input(candidate.tx_data.clone())
            .with_max_fee_per_gas(fee_cap)
            .with_max_priority_fee_per_gas(tip_cap)
            .with_value(candidate.value)
            .with_chain_id(self.chain_id);

        tx_request.set_from(from);

        match candidate.to {
            Some(to) => tx_request.set_to(to),
            None => tx_request = tx_request.into_create(),
        }

        // Step 5: Gas estimation.
        //
        // Always call estimate_gas to enforce intrinsic gas checks (e.g.
        // the 21,000 minimum for plain value transfers). When the caller
        // supplies an explicit gas_limit, it is used as a floor via max()
        // so the transaction never under-provisions gas.
        let estimated = tokio::time::timeout(
            self.config.network_timeout,
            self.provider.estimate_gas(tx_request.clone()),
        )
        .await
        .map_err(|_| TxManagerError::Rpc("estimate_gas timed out".into()))?
        .map_err(|e| RpcErrorClassifier::classify_rpc_error(&e.to_string()))?;
        let gas_limit = candidate.gas_limit.max(estimated);
        tx_request = tx_request.with_gas_limit(gas_limit);

        // Step 6: Assign nonce.
        let guard = self.nonce_manager.next_nonce().await?;
        tx_request = tx_request.with_nonce(guard.nonce());

        info!(
            nonce = guard.nonce(),
            gas_limit = %gas_limit,
            tip_cap = %tip_cap,
            fee_cap = %fee_cap,
            "transaction crafted",
        );

        // Step 7: Sign and encode.
        let sign_result =
            <TransactionRequest as TransactionBuilder<Ethereum>>::build(tx_request, &self.wallet)
                .await;

        match sign_result {
            Ok(envelope) => {
                // Consume the nonce (drop guard).
                drop(guard);
                Ok(PreparedTx {
                    raw_tx: Bytes::from(Encodable2718::encoded_2718(&envelope)),
                    gas_tip_cap: tip_cap,
                    gas_fee_cap: fee_cap,
                })
            }
            Err(e) => {
                // Roll back nonce on sign failure.
                guard.rollback();
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
    /// # Send timeout
    ///
    /// When `tx_send_timeout` is configured, the inner send loop is wrapped
    /// in a [`tokio::time::timeout`]. If the send exits with an error before
    /// any successful publish, the nonce manager is reset before returning.
    /// This prevents nonce gaps from pre-publish failures, including
    /// cancellation of `prepare()` after nonce acquisition but before the
    /// transaction reached the mempool.
    ///
    /// # Errors
    ///
    /// Returns the confirmed [`TransactionReceipt`] on success, or a
    /// [`TxManagerError`] on critical errors, timeout, or manager shutdown.
    async fn send_tx(&self, candidate: TxCandidate) -> SendResponse {
        if self.is_closed() {
            return Err(TxManagerError::ChannelClosed);
        }

        let send_state = Arc::new(SendState::new(self.config.safe_abort_nonce_too_low_count)?);

        // Set mempool deadline if configured.
        if !self.config.tx_not_in_mempool_timeout.is_zero() {
            send_state.set_mempool_deadline(Instant::now() + self.config.tx_not_in_mempool_timeout);
        }

        // Wrap the inner loop in a send timeout if configured.
        let result = if self.config.tx_send_timeout.is_zero() {
            self.send_tx_inner(&candidate, &send_state).await
        } else {
            tokio::time::timeout(
                self.config.tx_send_timeout,
                self.send_tx_inner(&candidate, &send_state),
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

        if Self::should_reset_nonce_on_send_error(&result, &send_state) {
            // Reset only if nothing was ever published. Once a transaction may
            // be pending, reusing the latest nonce could conflict with the
            // in-flight transaction.
            self.nonce_manager.reset().await;
        }

        result
    }

    fn should_reset_nonce_on_send_error<T>(
        result: &TxManagerResult<T>,
        send_state: &SendState,
    ) -> bool {
        match result {
            Ok(_) => false,
            // Always reset on timeout — the timeout may have cancelled
            // prepare() mid-flight during a fee bump, leaking a nonce
            // from the nonce manager's internal counter.
            Err(TxManagerError::SendTimeout) => true,
            // For other errors, reset only if nothing was ever published.
            // Once a transaction is pending, the next send_tx will re-sync
            // via the chain's pending nonce anyway.
            Err(_) => send_state.successful_publish_count() == 0,
        }
    }

    /// Inner send loop extracted from [`send_tx`](Self::send_tx) to allow
    /// optional timeout wrapping.
    async fn send_tx_inner(
        &self,
        candidate: &TxCandidate,
        send_state: &Arc<SendState>,
    ) -> SendResponse {
        // Initial transaction preparation. prepare() is NOT cancellation-safe,
        // so it runs to completion before entering the select loop.
        // The returned PreparedTx carries the actual on-wire fees, eliminating
        // the need for a separate suggest_gas_price_caps() call.
        let prepared = self.prepare(candidate, None).await?;
        let mut current_tip = prepared.gas_tip_cap;
        let mut current_fee_cap = prepared.gas_fee_cap;

        // Publish initial transaction.
        let mut last_tx_hash = self.publish_tx(send_state, &prepared.raw_tx, None).await?;

        // Receipt delivery channel — mpsc because fee bumps may spawn
        // new wait tasks with different tx hashes.
        let (receipt_tx, mut receipt_rx) = mpsc::channel::<TransactionReceipt>(1);

        // Spawn background receipt polling for the initial tx hash.
        Self::wait_for_tx(
            Arc::clone(send_state),
            self.provider.clone(),
            last_tx_hash,
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

            // Respond immediately to the should_bump_fees flag set by
            // process_send_error on retryable errors (e.g. Underpriced,
            // ReplacementUnderpriced), rather than waiting for the next
            // resubmission timer tick.
            //
            // Clear the flag before attempting the bump so that a failed
            // attempt (e.g. RPC timeout) does not immediately re-trigger
            // on the next loop iteration — instead, the loop falls through
            // to tokio::select! which waits for the resubmission timer or
            // a receipt, providing natural backoff. If a new retryable
            // error occurs later, process_send_error will re-set the flag.
            if send_state.should_bump_fees() {
                send_state.clear_bump_fees();
                if let Some(abort) = self
                    .try_fee_bump(
                        candidate,
                        send_state,
                        &receipt_tx,
                        &mut current_tip,
                        &mut current_fee_cap,
                        &mut last_tx_hash,
                    )
                    .await
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
                _ = bump_ticker.tick() => {}
                result = receipt_rx.recv() => {
                    match result {
                        Some(receipt) => {
                            info!(
                                tx_hash = %receipt.transaction_hash,
                                block = ?receipt.block_number,
                                "transaction confirmed",
                            );
                            return Ok(receipt);
                        }
                        None => {
                            // All senders dropped — should not happen in normal flow.
                            return Err(TxManagerError::ChannelClosed);
                        }
                    }
                }
            };

            // Bump tick fired — run fee bump logic.
            if let Some(abort) = self
                .try_fee_bump(
                    candidate,
                    send_state,
                    &receipt_tx,
                    &mut current_tip,
                    &mut current_fee_cap,
                    &mut last_tx_hash,
                )
                .await
            {
                return Err(abort);
            }
            bump_ticker.reset();
        }
    }

    /// Performs a fee bump attempt and applies the result to the tracked state.
    ///
    /// Returns `Some(error)` if the send loop must abort, `None` to continue.
    async fn try_fee_bump(
        &self,
        candidate: &TxCandidate,
        send_state: &Arc<SendState>,
        receipt_tx: &mpsc::Sender<TransactionReceipt>,
        current_tip: &mut u128,
        current_fee_cap: &mut u128,
        last_tx_hash: &mut B256,
    ) -> Option<TxManagerError> {
        let result = self
            .handle_fee_bump(
                candidate,
                send_state,
                receipt_tx,
                *current_tip,
                *current_fee_cap,
                *last_tx_hash,
            )
            .await;
        Self::apply_bump_result(result, current_tip, current_fee_cap, last_tx_hash)
    }

    /// Handles a single fee bump iteration.
    ///
    /// Queries fresh gas prices, applies [`FeeCalculator::update_fees`],
    /// checks fee limits, resets the nonce manager, rebuilds and re-publishes
    /// the transaction, and spawns a new receipt polling task.
    ///
    /// The bumped `(tip, fee_cap)` values are passed as fee overrides to
    /// [`prepare`](Self::prepare), which forwards them to
    /// [`craft_tx`](Self::craft_tx). There, each override is used as a
    /// floor via `max(network_fee, override)`, guaranteeing the replacement
    /// transaction meets geth's replacement thresholds even if network fees
    /// have dropped since the bump was calculated.
    ///
    /// Returns the updated `(tip, fee_cap, tx_hash)` on success. The
    /// returned tip and fee cap are taken directly from the [`PreparedTx`]
    /// returned by `prepare()`, reflecting the *actual* on-wire fees
    /// (which are guaranteed >= the bumped values) so that subsequent bump
    /// iterations use an accurate baseline.
    async fn handle_fee_bump(
        &self,
        candidate: &TxCandidate,
        send_state: &Arc<SendState>,
        receipt_tx: &mpsc::Sender<TransactionReceipt>,
        old_tip: u128,
        old_fee_cap: u128,
        last_tx_hash: B256,
    ) -> TxManagerResult<(u128, u128, B256)> {
        let caps = self.suggest_gas_price_caps().await?;

        // Derive the effective base fee from the fee cap and tip.
        let new_base_fee = FeeCalculator::base_fee_from_caps(caps.gas_fee_cap, caps.gas_tip_cap);

        let (bumped_tip, bumped_fee_cap) =
            FeeCalculator::update_fees(old_tip, old_fee_cap, caps.gas_tip_cap, new_base_fee, false);

        FeeCalculator::check_limits(
            bumped_fee_cap,
            caps.raw_gas_fee_cap,
            self.config.fee_limit_multiplier,
            self.config.fee_limit_threshold,
        )?;

        // Reset nonce manager so prepare() gets the same pending nonce.
        self.nonce_manager.reset().await;

        // Rebuild transaction with bumped fees as overrides. craft_tx()
        // takes max(network_fee, override) for each component, so the
        // replacement tx is guaranteed to satisfy geth's replacement
        // thresholds. The returned PreparedTx carries the actual on-wire
        // fees, eliminating the need for a post-hoc reconciliation query.
        let prepared = self
            .prepare_with_initial_caps(
                candidate,
                Some(FeeOverride::new(bumped_tip, bumped_fee_cap)),
                Some(caps),
            )
            .await?;

        let new_hash = self.publish_tx(send_state, &prepared.raw_tx, Some(last_tx_hash)).await?;

        // Record the bump and log only after the transaction has been
        // successfully published to avoid inflating the count on failure.
        send_state.record_fee_bump();
        info!(
            bump_count = %send_state.bump_count(),
            old_tip = %old_tip,
            new_tip = %prepared.gas_tip_cap,
            old_fee_cap = %old_fee_cap,
            new_fee_cap = %prepared.gas_fee_cap,
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

        Ok((prepared.gas_tip_cap, prepared.gas_fee_cap, new_hash))
    }

    /// Applies the result of a fee bump attempt, updating the tracked fee
    /// state on success or logging the error on failure.
    ///
    /// Returns `Some(error)` when the caller must abort the send loop
    /// (non-retryable error), or `None` when the loop should continue
    /// (success or retryable error).
    fn apply_bump_result(
        result: TxManagerResult<(u128, u128, B256)>,
        current_tip: &mut u128,
        current_fee_cap: &mut u128,
        last_tx_hash: &mut B256,
    ) -> Option<TxManagerError> {
        match result {
            Ok((new_tip, new_fee_cap, new_hash)) => {
                *current_tip = new_tip;
                *current_fee_cap = new_fee_cap;
                *last_tx_hash = new_hash;
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
                let classified = RpcErrorClassifier::classify_rpc_error(&e.to_string());

                // AlreadyKnown on resubmission is a success — the tx is in
                // the mempool from a prior publish. Return the previous hash.
                if classified.is_already_known() && send_state.successful_publish_count() > 0 {
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

                if classified.is_retryable() {
                    warn!(error = %classified, "publish failed, will retry");
                } else {
                    error!(error = %classified, "publish failed with non-retryable error");
                }

                Err(classified)
            }
            Err(_) => {
                let err = TxManagerError::Rpc("send_raw_transaction timed out".into());
                send_state.process_send_error(&err);
                warn!("publish timed out");
                Err(err)
            }
        }
    }

    /// Spawns a background task that polls for a transaction receipt and
    /// sends it through the provided channel once confirmed.
    ///
    /// The task calls [`wait_mined`](Self::wait_mined) in a loop, checking
    /// both confirmation status and manager shutdown via the shared `closed`
    /// flag.
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
    fn wait_for_tx(
        send_state: Arc<SendState>,
        provider: RootProvider,
        tx_hash: B256,
        config: TxManagerConfig,
        receipt_tx: mpsc::Sender<TransactionReceipt>,
        closed: Arc<AtomicBool>,
    ) {
        tokio::spawn(async move {
            debug!(tx_hash = %tx_hash, "starting receipt polling");

            let receipt = Self::wait_mined(&send_state, &provider, tx_hash, &config, &closed).await;
            if let Some(receipt) = receipt {
                // Best-effort send — if the receiver is dropped, the
                // send loop has already exited (e.g., another tx confirmed).
                let _ = receipt_tx.send(receipt).await;
            }

            debug!(tx_hash = %tx_hash, "receipt polling ended");
        });
    }

    /// Polls for a transaction receipt at `receipt_query_interval` until
    /// the transaction is mined and confirmed to the required depth.
    ///
    /// Returns `Some(receipt)` when the transaction reaches
    /// `num_confirmations` depth, or `None` if the manager is closed.
    async fn wait_mined(
        send_state: &SendState,
        provider: &RootProvider,
        tx_hash: B256,
        config: &TxManagerConfig,
        closed: &AtomicBool,
    ) -> Option<TransactionReceipt> {
        let mut poll_interval = tokio::time::interval(config.receipt_query_interval);

        loop {
            poll_interval.tick().await;

            // Check shutdown state each iteration to support cancellation.
            if closed.load(Ordering::Acquire) {
                debug!(tx_hash = %tx_hash, "manager closed, stopping receipt polling");
                return None;
            }

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
        provider: &RootProvider,
        tx_hash: B256,
        num_confirmations: u64,
        network_timeout: Duration,
    ) -> TxManagerResult<Option<TransactionReceipt>> {
        let receipt_opt =
            tokio::time::timeout(network_timeout, provider.get_transaction_receipt(tx_hash))
                .await
                .map_err(|_| TxManagerError::Rpc("get_transaction_receipt timed out".into()))?
                .map_err(|e| RpcErrorClassifier::classify_rpc_error(&e.to_string()))?;

        let receipt = match receipt_opt {
            Some(r) => r,
            None => {
                send_state.tx_not_mined(tx_hash);
                debug!(tx_hash = %tx_hash, "receipt not found, possible reorg");
                return Ok(None);
            }
        };

        // Receipt exists — record as mined regardless of confirmation depth.
        send_state.tx_mined(tx_hash);

        let tx_block = match receipt.block_number {
            Some(block) => block,
            None => {
                // Receipt without a block number (e.g., pending) — treat as not yet confirmed.
                return Ok(None);
            }
        };

        // Check confirmation depth: tx_block + num_confirmations <= tip + 1
        let tip_height = tokio::time::timeout(network_timeout, provider.get_block_number())
            .await
            .map_err(|_| TxManagerError::Rpc("get_block_number timed out".into()))?
            .map_err(|e| RpcErrorClassifier::classify_rpc_error(&e.to_string()))?;

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
                needed = %num_confirmations,
                "waiting for more confirmations",
            );
            Ok(None)
        }
    }
}

impl TxManager for SimpleTxManager {
    async fn send(&self, candidate: TxCandidate) -> SendResponse {
        self.send_tx(candidate).await
    }

    async fn send_async(&self, candidate: TxCandidate) -> SendHandle {
        let (tx, rx) = oneshot::channel();

        // Clone the manager for the spawned task. The `closed` flag is
        // Arc-wrapped, so the spawned task shares the same shutdown signal
        // as the original manager — calling `close()` on the original will
        // be observed by the background task.
        let manager = self.clone();

        tokio::spawn(async move {
            let result = manager.send_tx(candidate).await;
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
    use alloy_consensus::{TxEip1559, TxEnvelope};
    use alloy_eips::Decodable2718;
    use alloy_network::EthereumWallet;
    use alloy_node_bindings::Anvil;
    use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
    use alloy_provider::RootProvider;
    use alloy_signer_local::PrivateKeySigner;

    use super::SimpleTxManager;
    use rstest::rstest;

    use crate::{GasPriceCaps, TxCandidate, TxManagerConfig, TxManagerError};

    async fn setup() -> (SimpleTxManager, alloy_node_bindings::AnvilInstance) {
        let anvil = Anvil::new().spawn();
        let url = anvil.endpoint_url();
        let provider = RootProvider::new_http(url);
        let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
        let wallet = EthereumWallet::from(signer);
        let chain_id = anvil.chain_id();
        let manager = SimpleTxManager::new(provider, wallet, TxManagerConfig::default(), chain_id)
            .await
            .expect("should create manager");
        (manager, anvil)
    }

    fn decode_eip1559(raw: &Bytes) -> TxEip1559 {
        let envelope =
            TxEnvelope::decode_2718(&mut raw.as_ref()).expect("should decode as valid TxEnvelope");
        match envelope {
            TxEnvelope::Eip1559(signed) => signed.strip_signature(),
            other => panic!("expected EIP-1559, got {other:?}"),
        }
    }

    // ── apply_bump_result ─────────────────────────────────────────────

    #[rstest]
    #[case::success_updates_state(
        Ok((200, 2000, B256::with_last_byte(0x42))),
        false, 200, 2000, B256::with_last_byte(0x42),
    )]
    #[case::non_retryable_returns_abort(
        Err(TxManagerError::FeeLimitExceeded { fee: 500, ceiling: 100 }),
        true, 100, 1000, B256::ZERO,
    )]
    #[case::retryable_continues(
        Err(TxManagerError::Rpc("transient error".to_string())),
        false, 100, 1000, B256::ZERO,
    )]
    fn apply_bump_result(
        #[case] input: Result<(u128, u128, B256), TxManagerError>,
        #[case] abort_expected: bool,
        #[case] expected_tip: u128,
        #[case] expected_fee_cap: u128,
        #[case] expected_hash: B256,
    ) {
        let mut tip = 100u128;
        let mut fee_cap = 1000u128;
        let mut hash = B256::ZERO;

        let abort = SimpleTxManager::apply_bump_result(input, &mut tip, &mut fee_cap, &mut hash);

        assert_eq!(abort.is_some(), abort_expected);
        assert_eq!(tip, expected_tip);
        assert_eq!(fee_cap, expected_fee_cap);
        assert_eq!(hash, expected_hash);
    }

    #[rstest]
    #[case::error_before_first_publish(false, Err(TxManagerError::SendTimeout), true)]
    #[case::non_timeout_error_after_publish(true, Err(TxManagerError::ChannelClosed), false)]
    #[case::timeout_after_publish(true, Err(TxManagerError::SendTimeout), true)]
    #[case::success(false, Ok(()), false)]
    fn should_reset_nonce_on_send_error(
        #[case] has_publish: bool,
        #[case] result: crate::TxManagerResult<()>,
        #[case] expected: bool,
    ) {
        let send_state = crate::SendState::new(3).expect("should create send state");
        if has_publish {
            send_state.record_successful_publish();
        }
        assert_eq!(
            SimpleTxManager::should_reset_nonce_on_send_error(&result, &send_state),
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
        };

        let prepared = manager
            .prepare_with_initial_caps(&candidate, None, Some(caps.clone()))
            .await
            .expect("should prepare tx using supplied caps");
        let tx = decode_eip1559(&prepared.raw_tx);

        assert_eq!(tx.to, TxKind::Call(Address::with_last_byte(0x42)));
        assert_eq!(prepared.gas_tip_cap, caps.gas_tip_cap);
        assert_eq!(prepared.gas_fee_cap, caps.gas_fee_cap);
        assert_eq!(tx.max_priority_fee_per_gas, caps.gas_tip_cap);
        assert_eq!(tx.max_fee_per_gas, caps.gas_fee_cap);
    }
}
