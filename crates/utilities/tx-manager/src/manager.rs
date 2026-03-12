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
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use alloy_eips::{BlockNumberOrTag, Encodable2718};
use alloy_network::{Ethereum, EthereumWallet, NetworkWallet, TransactionBuilder};
use alloy_primitives::{Address, Bytes};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::TransactionRequest;
use backon::{ConstantBuilder, Retryable};
use tracing::{info, warn};

use crate::{
    FeeCalculator, GasPriceCaps, NonceManager, RpcErrorClassifier, SendHandle, SendResponse,
    TxCandidate, TxManager, TxManagerConfig, TxManagerError, TxManagerResult,
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
    const PREPARE_MAX_RETRIES: usize = 30;

    /// Fixed delay between retry attempts in [`Self::prepare`].
    const PREPARE_RETRY_DELAY: Duration = Duration::from_secs(2);

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
        let provider_chain_id = tokio::time::timeout(
            config.network_timeout,
            provider.get_chain_id(),
        )
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
    pub async fn suggest_gas_price_caps(
        &self,
        _candidate: &TxCandidate,
    ) -> TxManagerResult<GasPriceCaps> {
        // Query tip cap.
        let raw_tip_cap = tokio::time::timeout(
            self.config.network_timeout,
            self.provider.get_max_priority_fee_per_gas(),
        )
        .await
        .map_err(|_| TxManagerError::Rpc("get_max_priority_fee_per_gas timed out".into()))?
        .map_err(|e| RpcErrorClassifier::classify_rpc_error(&e.to_string()))?;

        // Query latest block for base fee.
        let latest_block = tokio::time::timeout(
            self.config.network_timeout,
            self.provider.get_block_by_number(BlockNumberOrTag::Latest),
        )
        .await
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
        let caps = self.suggest_gas_price_caps(candidate).await?;

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
