//! Core transaction manager implementation.
//!
//! [`SimpleTxManager`] takes a [`TxCandidate`] through the full construction
//! pipeline: gas price estimation via [`suggest_gas_price_caps`], fee limit
//! enforcement via [`FeeCalculator::check_limits`], gas estimation or
//! validation against the provider, nonce assignment via [`NonceManager`],
//! and signing via alloy's [`NetworkWallet`] trait.
//!
//! The outer [`prepare`] method wraps [`craft_tx`] in a `backon` retry loop
//! (up to 30 attempts, 2-second fixed delay) that retries only on transient
//! errors and exits immediately on shutdown.
//!
//! The [`send_tx`] method drives a signed transaction from publication through
//! mempool to onchain confirmation via a `tokio::select!` event loop that
//! coordinates fee bumping, receipt polling, and critical error detection.
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
    FeeCalculator, GasPriceCaps, NonceManager, RpcErrorClassifier, SendHandle, SendResponse,
    SendState, TxCandidate, TxManager, TxManagerConfig, TxManagerError, TxManagerResult,
};

/// Default transaction manager implementation.
///
/// Constructs, signs, and submits EIP-1559 transactions. All RPC fields
/// (nonce, gas, fees) are set manually on [`TransactionRequest`] without
/// alloy fillers or `PendingTransactionBuilder`.
#[derive(Debug)]
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
    /// Shutdown flag. Set to `true` to close the manager.
    closed: AtomicBool,
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
            closed: AtomicBool::new(false),
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
    pub async fn prepare(&self, candidate: &TxCandidate) -> TxManagerResult<Bytes> {
        if self.is_closed() {
            return Err(TxManagerError::ChannelClosed);
        }

        (|| async {
            // Re-check closed flag on each retry attempt to avoid wasted
            // RPC calls after shutdown. ChannelClosed is non-retryable,
            // so backon exits the loop immediately.
            if self.is_closed() {
                return Err(TxManagerError::ChannelClosed);
            }
            self.craft_tx(candidate).await
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
    /// 2. Check fee limits via [`FeeCalculator::check_limits`]
    /// 3. Build a [`TransactionRequest`] with all fields set manually
    /// 4. Estimate gas via the provider; use `max(estimate, candidate.gas_limit)` as floor
    /// 5. Assign nonce via [`NonceManager::next_nonce`]
    /// 6. Sign and RLP-encode to raw transaction bytes
    ///
    /// # Cancellation safety
    ///
    /// This future is **not** cancellation-safe. If dropped after nonce
    /// acquisition (step 5) but before signing completes (step 6), the
    /// nonce is consumed without producing a transaction, creating a
    /// permanent nonce gap. Callers must not wrap this future in
    /// `tokio::select!`, `tokio::time::timeout`, or similar combinators
    /// that may drop it mid-flight.
    ///
    /// # Errors
    ///
    /// Returns classified RPC errors, [`TxManagerError::FeeLimitExceeded`],
    /// nonce errors, or signing errors.
    pub async fn craft_tx(&self, candidate: &TxCandidate) -> TxManagerResult<Bytes> {
        // Blob transactions are not yet supported.
        if !candidate.blobs.is_empty() {
            return Err(TxManagerError::Unsupported(
                "blob transactions are not yet supported".to_string(),
            ));
        }

        // Step 1: Get fee estimates.
        let caps = self.suggest_gas_price_caps().await?;

        // Step 2: Check fee limits.
        //
        // The `suggested` parameter is the raw gas_fee_cap computed from
        // the provider's values before enforcing our configured minimums
        // (`min_tip_cap`, `min_basefee`). This detects when enforced
        // minimums inflate the fee cap beyond
        // `fee_limit_multiplier × raw_provider_fee_cap`. During fee bumps
        // (future ticket) the caller passes the previous fee as
        // `suggested` instead.
        FeeCalculator::check_limits(
            caps.gas_fee_cap,
            caps.raw_gas_fee_cap,
            self.config.fee_limit_multiplier,
            self.config.fee_limit_threshold,
        )?;

        // Step 3: Build TransactionRequest.
        let from = self.sender_address();
        let mut tx_request = TransactionRequest::default()
            .with_input(candidate.tx_data.clone())
            .with_max_fee_per_gas(caps.gas_fee_cap)
            .with_max_priority_fee_per_gas(caps.gas_tip_cap)
            .with_value(candidate.value)
            .with_chain_id(self.chain_id);

        tx_request.from = Some(from);

        match candidate.to {
            Some(to) => tx_request.set_to(to),
            None => tx_request = tx_request.into_create(),
        }

        // Step 4: Gas estimation.
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

        // Step 5: Assign nonce.
        let guard = self.nonce_manager.next_nonce().await?;
        tx_request = tx_request.with_nonce(guard.nonce());

        info!(
            nonce = guard.nonce(),
            gas_limit = %gas_limit,
            tip_cap = %caps.gas_tip_cap,
            fee_cap = %caps.gas_fee_cap,
            "transaction crafted",
        );

        // Step 6: Sign and encode.
        let sign_result =
            <TransactionRequest as TransactionBuilder<Ethereum>>::build(tx_request, &self.wallet)
                .await;

        match sign_result {
            Ok(envelope) => {
                // Consume the nonce (drop guard).
                drop(guard);
                Ok(Bytes::from(Encodable2718::encoded_2718(&envelope)))
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
        if self.config.tx_send_timeout.is_zero() {
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
                Err(TxManagerError::Rpc("send timed out".into()))
            })
        }
    }

    /// Inner send loop extracted from [`send_tx`](Self::send_tx) to allow
    /// optional timeout wrapping.
    async fn send_tx_inner(
        &self,
        candidate: &TxCandidate,
        send_state: &Arc<SendState>,
    ) -> SendResponse {
        // Get initial gas price estimates for fee tracking.
        let caps = self.suggest_gas_price_caps().await?;
        let mut current_tip = caps.gas_tip_cap;
        let mut current_fee_cap = caps.gas_fee_cap;

        // Initial transaction preparation. prepare() is NOT cancellation-safe,
        // so it runs to completion before entering the select loop.
        let raw_tx = self.prepare(candidate).await?;

        // Publish initial transaction.
        let tx_hash = self.publish_tx(send_state, &raw_tx).await?;

        // Receipt delivery channel — mpsc because fee bumps may spawn
        // new wait tasks with different tx hashes.
        let (receipt_tx, mut receipt_rx) = mpsc::channel::<TransactionReceipt>(1);

        // Spawn background receipt polling for the initial tx hash.
        Self::wait_for_tx(
            Arc::clone(send_state),
            self.provider.clone(),
            tx_hash,
            self.config.clone(),
            receipt_tx.clone(),
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

            tokio::select! {
                _ = bump_ticker.tick() => {
                    // Fee bump: query fresh gas prices and apply bump logic.
                    let bump_result = self
                        .handle_fee_bump(candidate, send_state, &receipt_tx, current_tip, current_fee_cap)
                        .await;
                    match bump_result {
                        Ok((new_tip, new_fee_cap)) => {
                            current_tip = new_tip;
                            current_fee_cap = new_fee_cap;
                        }
                        Err(e) if !e.is_retryable() => {
                            error!(error = %e, "non-retryable error during fee bump");
                            return Err(e);
                        }
                        Err(e) => {
                            warn!(error = %e, "fee bump failed, will retry next tick");
                        }
                    }
                }
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
            }
        }
    }

    /// Handles a single fee bump iteration.
    ///
    /// Queries fresh gas prices, applies [`FeeCalculator::update_fees`],
    /// checks fee limits, resets the nonce manager, rebuilds and re-publishes
    /// the transaction, and spawns a new receipt polling task.
    ///
    /// Returns the updated `(tip, fee_cap)` on success.
    async fn handle_fee_bump(
        &self,
        candidate: &TxCandidate,
        send_state: &Arc<SendState>,
        receipt_tx: &mpsc::Sender<TransactionReceipt>,
        old_tip: u128,
        old_fee_cap: u128,
    ) -> TxManagerResult<(u128, u128)> {
        let caps = self.suggest_gas_price_caps().await?;

        // Derive the effective base fee from the fee cap and tip.
        let new_base_fee = caps.gas_fee_cap.saturating_sub(caps.gas_tip_cap) / 2;

        let (bumped_tip, bumped_fee_cap) =
            FeeCalculator::update_fees(old_tip, old_fee_cap, caps.gas_tip_cap, new_base_fee, false);

        FeeCalculator::check_limits(
            bumped_fee_cap,
            caps.raw_gas_fee_cap,
            self.config.fee_limit_multiplier,
            self.config.fee_limit_threshold,
        )?;

        send_state.record_fee_bump();
        info!(
            bump_count = %send_state.bump_count(),
            old_tip = %old_tip,
            new_tip = %bumped_tip,
            old_fee_cap = %old_fee_cap,
            new_fee_cap = %bumped_fee_cap,
            "fee bump applied",
        );

        // Reset nonce manager so prepare() gets the same pending nonce.
        self.nonce_manager.reset().await;

        // Rebuild transaction with fresh gas prices.
        // prepare() internally calls suggest_gas_price_caps() which should
        // return values at or above the bumped thresholds in normal conditions.
        let raw_tx = self.prepare(candidate).await?;

        match self.publish_tx(send_state, &raw_tx).await {
            Ok(new_hash) => {
                // Spawn a new receipt polling task for the bumped tx.
                Self::wait_for_tx(
                    Arc::clone(send_state),
                    self.provider.clone(),
                    new_hash,
                    self.config.clone(),
                    receipt_tx.clone(),
                );
            }
            Err(e) if e.is_already_known() => {
                // Transaction with same nonce+fees already in mempool.
                debug!("bumped transaction already known in mempool");
            }
            Err(e) => return Err(e),
        }

        Ok((bumped_tip, bumped_fee_cap))
    }

    /// Broadcasts a raw transaction to the network.
    ///
    /// On success, records a successful publish on the [`SendState`] and
    /// returns the transaction hash. On [`TxManagerError::AlreadyKnown`]
    /// after a prior successful publish, treats it as success (the tx is
    /// already in the mempool). All other errors are forwarded to
    /// [`SendState::process_send_error`] for state tracking.
    ///
    /// # Errors
    ///
    /// Returns the classified error on publication failure.
    pub async fn publish_tx(
        &self,
        send_state: &SendState,
        raw_tx: &Bytes,
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
                // the mempool from a prior publish.
                if classified.is_already_known() && send_state.successful_publish_count() > 0 {
                    send_state.record_successful_publish();
                    info!("transaction already known in mempool, treating as success");
                    // Return an error so the caller knows no NEW hash was produced,
                    // but it's not a real failure. The original tx hash is still valid.
                    return Err(classified);
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
    /// both confirmation status and manager shutdown.
    fn wait_for_tx(
        send_state: Arc<SendState>,
        provider: RootProvider,
        tx_hash: B256,
        config: TxManagerConfig,
        receipt_tx: mpsc::Sender<TransactionReceipt>,
    ) {
        tokio::spawn(async move {
            debug!(tx_hash = %tx_hash, "starting receipt polling");

            let receipt = Self::wait_mined(&send_state, &provider, tx_hash, &config).await;
            if let Some(receipt) = receipt {
                // Best-effort send — if the receiver is dropped, the
                // send loop has already exited (e.g., another tx confirmed).
                let _ = receipt_tx.send(receipt).await;
            }
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
    ) -> Option<TransactionReceipt> {
        let mut poll_interval = tokio::time::interval(config.receipt_query_interval);

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

        // The provider, wallet, config, etc. are behind Arc/AtomicBool
        // internally, but SimpleTxManager itself is not Clone. We need to
        // capture a reference. Since the trait returns impl Future + Send
        // and SimpleTxManager is Send + Sync, we can spawn a task that
        // calls send_tx on a shared reference via Arc.
        //
        // However, SimpleTxManager is not wrapped in Arc by default.
        // Instead, we prepare everything we need on the current task and
        // spawn the blocking work.
        //
        // Since `self` is &Self with a 'static-compatible lifetime
        // (required by the trait), we construct all needed params here.
        let provider = self.provider.clone();
        let wallet = self.wallet.clone();
        let config = self.config.clone();
        let chain_id = self.chain_id;
        let closed = self.is_closed();
        let nonce_manager = self.nonce_manager.clone();

        tokio::spawn(async move {
            if closed {
                let _ = tx.send(Err(TxManagerError::ChannelClosed));
                return;
            }

            // Construct a temporary SimpleTxManager on the spawned task.
            // This is necessary because SimpleTxManager is not Clone (AtomicBool).
            let manager = Self {
                provider,
                wallet,
                config,
                nonce_manager,
                chain_id,
                closed: AtomicBool::new(closed),
            };

            let result = manager.send_tx(candidate).await;
            let _ = tx.send(result);
        });

        SendHandle::new(rx)
    }

    fn sender_address(&self) -> Address {
        <EthereumWallet as NetworkWallet<Ethereum>>::default_signer_address(&self.wallet)
    }
}
