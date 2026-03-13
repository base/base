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
//! [`TransactionRequest`]: alloy_rpc_types_eth::TransactionRequest

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_eips::{BlockNumberOrTag, Encodable2718};
use alloy_network::{Ethereum, EthereumWallet, NetworkWallet, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::{TransactionReceipt, TransactionRequest};
use backon::{ConstantBuilder, Retryable};
use tokio::{sync::mpsc, time::Instant};
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
    /// Shutdown flag shared across the manager and any spawned background
    /// tasks. Wrapped in [`Arc`] so that calling [`close`](Self::close)
    /// on the original manager is observed by tasks spawned via
    /// [`send_async`](Self::send_async) and `wait_for_tx`.
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
        (|| async {
            // Re-check closed flag on each retry attempt to avoid wasted
            // RPC calls after shutdown. ChannelClosed is non-retryable,
            // so backon exits the loop immediately.
            if self.is_closed() {
                return Err(TxManagerError::ChannelClosed);
            }
            self.craft_tx(candidate, fee_overrides).await
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
        // Blob transactions are not yet supported.
        if !candidate.blobs.is_empty() {
            return Err(TxManagerError::Unsupported(
                "blob transactions are not yet supported".to_string(),
            ));
        }

        // Step 1: Get fee estimates.
        let caps = self.suggest_gas_price_caps().await?;

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
        provider: RootProvider,
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
        provider: &RootProvider,
        tx_hash: B256,
        config: &TxManagerConfig,
        closed: &AtomicBool,
    ) -> Option<TransactionReceipt> {
        let deadline = Instant::now() + config.confirmation_timeout;
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
        provider: &RootProvider,
        tx_hash: B256,
        num_confirmations: u64,
        network_timeout: Duration,
    ) -> TxManagerResult<Option<TransactionReceipt>> {
        // Fetch tip *before* the receipt so that a reorg between the two
        // calls can only undercount confirmations (safe), never overcount.
        let tip_height = tokio::time::timeout(network_timeout, provider.get_block_number())
            .await
            .map_err(|_| TxManagerError::Rpc("get_block_number timed out".into()))?
            .map_err(|e| RpcErrorClassifier::classify_rpc_error(&e.to_string()))?;

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

        let tx_block = match receipt.block_number {
            Some(block) => block,
            None => {
                // Receipt without a block number (e.g., pending) — treat as not yet confirmed.
                return Ok(None);
            }
        };

        // Receipt exists with a block number — record as mined.
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

impl TxManager for SimpleTxManager {
    async fn send(&self, _candidate: TxCandidate) -> SendResponse {
        todo!("SimpleTxManager::send — requires receipt polling (separate ticket)")
    }

    async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
        todo!("SimpleTxManager::send_async — requires receipt polling (separate ticket)")
    }

    fn sender_address(&self) -> Address {
        <EthereumWallet as NetworkWallet<Ethereum>>::default_signer_address(&self.wallet)
    }
}
