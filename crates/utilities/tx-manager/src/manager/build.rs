//! Transaction estimation, construction, signing, and fee replacement.

use std::{fmt::Debug, sync::Arc, time::Duration};

#[cfg(test)]
use alloy_consensus::TxEnvelope;
#[cfg(test)]
use alloy_eips::Decodable2718;
use alloy_eips::{BlockNumberOrTag, Encodable2718, eip7594::BlobTransactionSidecarEip7594};
use alloy_network::{
    Ethereum, EthereumWallet, Network, NetworkTransactionBuilder, NetworkWallet,
    TransactionBuilder, TransactionBuilderError,
};
use alloy_primitives::{B256, Bytes};
use alloy_provider::Provider;
use alloy_rpc_types_eth::TransactionRequest;
use alloy_transport::TransportError;
use base_runtime::{Runtime, RuntimeTimeout};
use tracing::{error, info, warn};

use super::pending::ReplacementReason;
use crate::{
    TxCandidate, TxManagerConfig, TxManagerError, TxManagerResult, TxMetrics,
    blob::{BlobTxBuilder, MAX_BLOBS_PER_TX},
    error::RpcErrorClassifier,
    fees::{BumpedFees, FeeCalculator, FeeOverride, GasPriceCaps},
};

/// Number of wei in one gwei for metrics conversion.
pub const WEI_PER_GWEI: f64 = 1_000_000_000.0;

/// A signed transaction and the construction values used on wire.
#[derive(Debug, Clone)]
pub struct PreparedTx {
    /// EIP-2718 encoded signed transaction bytes.
    pub raw_tx: Bytes,
    /// Canonical transaction hash, excluding pooled blob sidecar encoding.
    pub tx_hash: B256,
    /// Maximum priority fee per gas.
    pub gas_tip_cap: u128,
    /// Maximum total fee per gas.
    pub gas_fee_cap: u128,
    /// Blob fee cap, or `None` for a type-2 transaction.
    pub blob_fee_cap: Option<u128>,
    /// Gas limit used in the signed transaction.
    pub gas_limit: u64,
    /// Assigned account nonce.
    pub nonce: u64,
    /// Cached sidecar reused by replacement versions.
    pub sidecar: Option<Arc<BlobTransactionSidecarEip7594>>,
}

impl PreparedTx {
    /// Decodes the signed bytes into an alloy transaction envelope.
    #[cfg(test)]
    fn to_envelope(&self) -> Result<TxEnvelope, alloy_eips::eip2718::Eip2718Error> {
        TxEnvelope::decode_2718(&mut self.raw_tx.as_ref())
    }

    /// Returns fee floors that preserve all on-wire values.
    pub const fn fee_floor(&self) -> FeeOverride {
        FeeOverride {
            gas_tip_cap: self.gas_tip_cap,
            gas_fee_cap: self.gas_fee_cap,
            blob_fee_cap: self.blob_fee_cap,
            gas_limit_floor: self.gas_limit,
        }
    }
}

/// Stateless transaction construction service used by coordinator workers.
#[derive(Debug, Clone)]
pub struct TxBuilder<P, R> {
    /// Chain reader used for estimation and fee inputs.
    provider: P,
    /// Runtime supplying bounded RPC execution and retry delays.
    runtime: R,
    /// Wallet that signs every managed transaction version.
    wallet: EthereumWallet,
    /// Fee limits and network timeout policy.
    config: TxManagerConfig,
    /// Chain ID encoded into every signed transaction.
    chain_id: u64,
    /// Metrics sink for fee, nonce, and RPC observations.
    metrics: Arc<dyn TxMetrics>,
}

impl<P, R> TxBuilder<P, R>
where
    P: Provider + Clone + Debug + Send + Sync + 'static,
    R: Runtime,
{
    /// Maximum retries after the first construction attempt.
    pub const PREPARE_MAX_RETRIES: usize = 30;

    /// Delay between construction retries.
    pub const PREPARE_RETRY_DELAY: Duration = Duration::from_secs(2);

    /// Creates a transaction builder.
    pub fn new(
        provider: P,
        runtime: R,
        wallet: EthereumWallet,
        config: TxManagerConfig,
        chain_id: u64,
        metrics: Arc<dyn TxMetrics>,
    ) -> Self {
        Self { provider, runtime, wallet, config, chain_id, metrics }
    }

    /// Constructs an initial signed transaction at an explicitly assigned nonce.
    pub async fn prepare_initial(
        &self,
        candidate: &TxCandidate,
        nonce: u64,
    ) -> TxManagerResult<PreparedTx> {
        self.prepare_with(candidate, nonce, None, None, None).await
    }

    /// Constructs a replacement or re-signed version from an existing one.
    pub async fn prepare_replacement(
        &self,
        candidate: &TxCandidate,
        base: &PreparedTx,
        nonce: u64,
        reason: ReplacementReason,
    ) -> TxManagerResult<PreparedTx> {
        match reason {
            ReplacementReason::Resign => {
                // Resigning after NonceTooLow changes only the nonce. Existing
                // on-wire fees remain floors and the expensive sidecar is reused.
                self.prepare_with(
                    candidate,
                    nonce,
                    Some(base.fee_floor()),
                    None,
                    base.sidecar.clone(),
                )
                .await
            }
            ReplacementReason::FeeBump | ReplacementReason::Cancel => {
                // Fee and cancellation replacements must satisfy both network
                // suggestions and the protocol replacement threshold.
                let bumped = self
                    .increase_gas_price(
                        candidate,
                        base.gas_tip_cap,
                        base.gas_fee_cap,
                        base.blob_fee_cap,
                    )
                    .await?;
                self.prepare_with(
                    candidate,
                    nonce,
                    Some(bumped.to_fee_override(base.gas_limit)),
                    Some(bumped.caps),
                    base.sidecar.clone(),
                )
                .await
            }
        }
    }

    /// Computes fees for a valid mempool replacement.
    pub async fn increase_gas_price(
        &self,
        candidate: &TxCandidate,
        old_tip: u128,
        old_fee_cap: u128,
        old_blob_fee_cap: Option<u128>,
    ) -> TxManagerResult<BumpedFees> {
        let is_blob = candidate.is_blob();
        if old_blob_fee_cap.is_some() != is_blob {
            return Err(TxManagerError::Unsupported(
                "replacement candidate and previous blob fee cap disagree".to_string(),
            ));
        }

        let caps = self.suggest_gas_price_caps_for(is_blob).await?;
        let base_fee = FeeCalculator::base_fee_from_caps(caps.gas_fee_cap, caps.gas_tip_cap);
        let (gas_tip_cap, gas_fee_cap) =
            FeeCalculator::update_fees(old_tip, old_fee_cap, caps.gas_tip_cap, base_fee, is_blob);
        let blob_fee_cap = old_blob_fee_cap.map(|old| {
            let threshold = FeeCalculator::calc_threshold_value(old, true);
            caps.blob_fee_cap.map_or(threshold, |network| threshold.max(network))
        });
        self.check_fee_limits(gas_fee_cap, blob_fee_cap, &caps)?;

        Ok(BumpedFees { gas_tip_cap, gas_fee_cap, blob_fee_cap, caps })
    }

    /// Retries bounded transaction construction on transient RPC failures.
    ///
    /// `initial_caps` is consumed by the first attempt so retries refresh
    /// network fees instead of repeatedly using a stale suggestion.
    pub async fn prepare_with(
        &self,
        candidate: &TxCandidate,
        nonce: u64,
        fee_override: Option<FeeOverride>,
        mut initial_caps: Option<GasPriceCaps>,
        sidecar: Option<Arc<BlobTransactionSidecarEip7594>>,
    ) -> TxManagerResult<PreparedTx> {
        for retry in 0..=Self::PREPARE_MAX_RETRIES {
            match self
                .craft(candidate, nonce, fee_override, initial_caps.take(), sidecar.clone())
                .await
            {
                Ok(prepared) => return Ok(prepared),
                Err(error) if error.is_retryable() && retry < Self::PREPARE_MAX_RETRIES => {
                    warn!(
                        error_kind = error.kind(),
                        retry = retry + 1,
                        max_retries = Self::PREPARE_MAX_RETRIES,
                        delay = ?Self::PREPARE_RETRY_DELAY,
                        "retrying transaction construction",
                    );
                    self.runtime.sleep(Self::PREPARE_RETRY_DELAY).await;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("bounded construction loop always returns")
    }

    /// Validates, prices, estimates, signs, and encodes one transaction version.
    pub async fn craft(
        &self,
        candidate: &TxCandidate,
        nonce: u64,
        fee_override: Option<FeeOverride>,
        caps: Option<GasPriceCaps>,
        cached_sidecar: Option<Arc<BlobTransactionSidecarEip7594>>,
    ) -> TxManagerResult<PreparedTx> {
        // Phase 1: reject transaction shapes that cannot be represented by the
        // selected EIP-1559 or EIP-4844 envelope.
        let is_blob = candidate.is_blob();
        if is_blob && candidate.to.is_none() {
            return Err(TxManagerError::Unsupported(
                "blob transactions must have a recipient address".to_string(),
            ));
        }
        if is_blob && candidate.blobs.len() > MAX_BLOBS_PER_TX {
            return Err(TxManagerError::Unsupported(format!(
                "blob count {} exceeds maximum {} per transaction",
                candidate.blobs.len(),
                MAX_BLOBS_PER_TX,
            )));
        }

        // Phase 2: merge fresh network suggestions with immutable floors from
        // an earlier version, then enforce the configured fee ceiling.
        let caps = match caps {
            Some(caps) => caps,
            None => self.suggest_gas_price_caps_for(is_blob).await?,
        };
        let (tip_cap, fee_cap) =
            fee_override.as_ref().map_or((caps.gas_tip_cap, caps.gas_fee_cap), |floor| {
                (caps.gas_tip_cap.max(floor.gas_tip_cap), caps.gas_fee_cap.max(floor.gas_fee_cap))
            });
        let blob_fee_cap = if is_blob {
            let network = caps.blob_fee_cap.ok_or_else(|| {
                TxManagerError::Unsupported(
                    "blob fee cap missing while constructing a blob transaction".to_string(),
                )
            })?;
            Some(network.max(fee_override.as_ref().and_then(|f| f.blob_fee_cap).unwrap_or(0)))
        } else {
            None
        };
        self.check_fee_limits(fee_cap, blob_fee_cap, &caps)?;

        // Phase 3: construct the unsigned request. Blob hashes and fee fields
        // are populated before estimation so the provider evaluates the real
        // transaction type.
        let from =
            <EthereumWallet as NetworkWallet<Ethereum>>::default_signer_address(&self.wallet);
        let mut request = TransactionRequest::default()
            .with_max_fee_per_gas(fee_cap)
            .with_max_priority_fee_per_gas(tip_cap)
            .with_value(candidate.value)
            .with_chain_id(self.chain_id)
            .with_nonce(nonce);
        request.input = Some(candidate.tx_data.clone()).into();
        request.set_from(from);
        match candidate.to {
            Some(to) => request.set_to(to),
            None => request = request.into_create(),
        }

        let built_sidecar = if is_blob {
            let sidecar = match cached_sidecar {
                Some(sidecar) => sidecar,
                None => Arc::new(BlobTxBuilder::build_sidecar(&candidate.blobs)?),
            };
            request.sidecar = Some((*sidecar).clone().into());
            request.populate_blob_hashes();
            request.max_fee_per_blob_gas = blob_fee_cap;
            Some(sidecar)
        } else {
            None
        };

        // Phase 4: estimate without serializing the large sidecar through RPC,
        // then restore it for local signing. Explicit gas is a floor, never a
        // replacement for the provider estimate.
        let sidecar_stash = request.sidecar.take();
        let estimated = RuntimeTimeout::run(
            &self.runtime,
            self.config.network_timeout,
            self.provider.estimate_gas(request.clone()),
        )
        .await
        .map_err(|_| self.rpc_error("estimate_gas timed out"))?
        .map_err(|error| {
            let error = self.classify_rpc(&error);
            if matches!(error, TxManagerError::ExecutionReverted { .. }) {
                error!(error_kind = error.kind(), "gas estimation reverted");
            }
            error
        })?;
        request.sidecar = sidecar_stash;
        let gas_limit = candidate
            .gas_limit
            .max(fee_override.as_ref().map_or(0, |floor| floor.gas_limit_floor))
            .max(estimated);
        request = request.with_gas_limit(gas_limit);

        // Phase 5: sign exactly once and retain the canonical envelope hash;
        // pooled blob encoding must never be used as the transaction identity.
        let envelope: Result<<Ethereum as Network>::TxEnvelope, TransactionBuilderError<Ethereum>> =
            <TransactionRequest as NetworkTransactionBuilder<Ethereum>>::build(
                request,
                &self.wallet,
            )
            .await;
        let envelope = envelope.map_err(|error| TxManagerError::Sign(error.to_string()))?;
        let tx_hash = *envelope.tx_hash();
        info!(
            nonce,
            gas_limit,
            tip_cap,
            fee_cap,
            blob_fee_cap = ?blob_fee_cap,
            "transaction constructed and signed",
        );
        self.metrics.record_tx_max_fee(gas_limit as f64 * (fee_cap as f64 / WEI_PER_GWEI));
        self.metrics.record_current_nonce(nonce);

        Ok(PreparedTx {
            raw_tx: Bytes::from(Encodable2718::encoded_2718(&envelope)),
            tx_hash,
            gas_tip_cap: tip_cap,
            gas_fee_cap: fee_cap,
            blob_fee_cap,
            gas_limit,
            nonce,
            sidecar: built_sidecar,
        })
    }

    /// Fetches network fee inputs concurrently and applies configured minima.
    pub async fn suggest_gas_price_caps_for(&self, is_blob: bool) -> TxManagerResult<GasPriceCaps> {
        // Tip and latest block are always required. Blob base fee is requested
        // only for type-3 construction.
        let tip = RuntimeTimeout::run(
            &self.runtime,
            self.config.network_timeout,
            self.provider.get_max_priority_fee_per_gas(),
        );
        let block = RuntimeTimeout::run(
            &self.runtime,
            self.config.network_timeout,
            self.provider.get_block_by_number(BlockNumberOrTag::Latest),
        );
        let (tip, block, blob_fee) = if is_blob {
            let blob_fee = RuntimeTimeout::run(
                &self.runtime,
                self.config.network_timeout,
                self.provider.get_blob_base_fee(),
            );
            let (tip, block, blob_fee) = tokio::join!(tip, block, blob_fee);
            (tip, block, Some(blob_fee))
        } else {
            let (tip, block) = tokio::join!(tip, block);
            (tip, block, None)
        };

        // Preserve raw caps for fee-limit checks before applying operational
        // floors such as min tip, base fee, and blob fee.
        let raw_tip = tip
            .map_err(|_| self.rpc_error("get_max_priority_fee_per_gas timed out"))?
            .map_err(|error| self.classify_rpc(&error))?;
        let block = block
            .map_err(|_| self.rpc_error("get_block_by_number timed out"))?
            .map_err(|error| self.classify_rpc(&error))?
            .ok_or_else(|| self.rpc_error("latest block not found"))?;
        let raw_base_fee = u128::from(
            block
                .header
                .base_fee_per_gas
                .ok_or_else(|| self.rpc_error("base fee not available"))?,
        );
        let raw_gas_fee_cap = FeeCalculator::calc_gas_fee_cap(raw_base_fee, raw_tip);
        let tip_cap = raw_tip.max(self.config.min_tip_cap);
        let base_fee = raw_base_fee.max(self.config.min_basefee);
        let gas_fee_cap = FeeCalculator::calc_gas_fee_cap(base_fee, tip_cap);
        self.metrics.record_basefee(base_fee as f64 / WEI_PER_GWEI);
        self.metrics.record_tipcap(tip_cap as f64 / WEI_PER_GWEI);

        let (blob_fee_cap, raw_blob_fee_cap) = match blob_fee {
            Some(blob_fee) => {
                let raw = blob_fee
                    .map_err(|_| self.rpc_error("get_blob_base_fee timed out"))?
                    .map_err(|error| self.classify_rpc(&error))?;
                let raw_cap = FeeCalculator::calc_blob_fee_cap(raw);
                let cap = FeeCalculator::calc_blob_fee_cap(raw.max(self.config.min_blob_fee));
                self.metrics.record_blob_fee(cap as f64 / WEI_PER_GWEI);
                (Some(cap), Some(raw_cap))
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

    /// Enforces regular and blob fee ceilings against raw network suggestions.
    pub fn check_fee_limits(
        &self,
        gas_fee_cap: u128,
        blob_fee_cap: Option<u128>,
        caps: &GasPriceCaps,
    ) -> TxManagerResult<()> {
        FeeCalculator::check_limits(
            gas_fee_cap,
            caps.raw_gas_fee_cap,
            self.config.fee_limit_multiplier,
            self.config.fee_limit_threshold,
        )?;
        if let Some(blob_fee_cap) = blob_fee_cap {
            let raw = caps.raw_blob_fee_cap.ok_or_else(|| {
                TxManagerError::Unsupported(
                    "raw blob fee cap missing while checking fee limits".to_string(),
                )
            })?;
            FeeCalculator::check_limits(
                blob_fee_cap,
                raw,
                self.config.fee_limit_multiplier,
                self.config.fee_limit_threshold,
            )?;
        }
        Ok(())
    }

    /// Classifies a transport error and records infrastructure failures.
    pub fn classify_rpc(&self, error: &TransportError) -> TxManagerError {
        let classified = RpcErrorClassifier::classify_rpc_error(error);
        if classified.is_rpc_error() {
            self.metrics.record_rpc_error();
        }
        classified
    }

    /// Creates a sanitized local RPC error and records it.
    pub fn rpc_error(&self, message: &str) -> TxManagerError {
        self.metrics.record_rpc_error();
        TxManagerError::Rpc(message.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::{SignableTransaction, TxEip1559, TxEip4844Variant, TxEnvelope};
    use alloy_eips::{eip4844::Blob, eip7594::CELLS_PER_EXT_BLOB};
    use alloy_network::{EthereumWallet, TxSigner};
    use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
    use alloy_provider::{RootProvider, builder as provider_builder, mock::Asserter};
    use alloy_rpc_types_eth::Block;
    use alloy_signer_local::PrivateKeySigner;
    use async_trait::async_trait;
    use base_runtime::TokioRuntime;

    use super::*;
    use crate::NoopTxMetrics;

    const TEST_RECIPIENT: Address = Address::with_last_byte(0x42);
    const TEST_CHAIN_ID: u64 = 1;

    fn test_signer() -> PrivateKeySigner {
        PrivateKeySigner::from_slice(&[1_u8; 32]).expect("valid test key")
    }

    fn test_block() -> Block<alloy_rpc_types_eth::Transaction> {
        let mut block: Block<alloy_rpc_types_eth::Transaction> = Block::default();
        block.header.inner.base_fee_per_gas = Some(1_000_000_000);
        block
    }

    fn push_fee_inputs(asserter: &Asserter, is_blob: bool) {
        asserter.push_success(&"0x3b9aca00");
        asserter.push_success(&test_block());
        if is_blob {
            asserter.push_success(&"0x1");
        }
    }

    fn push_gas_estimate(asserter: &Asserter) {
        asserter.push_success(&"0x5208");
    }

    fn test_builder(asserter: Asserter) -> TxBuilder<RootProvider, TokioRuntime> {
        TxBuilder::new(
            provider_builder().connect_mocked_client(asserter),
            TokioRuntime::new(),
            EthereumWallet::from(test_signer()),
            TxManagerConfig::default(),
            TEST_CHAIN_ID,
            Arc::new(NoopTxMetrics),
        )
    }

    fn value_transfer(value: u64) -> TxCandidate {
        TxCandidate { to: Some(TEST_RECIPIENT), value: U256::from(value), ..Default::default() }
    }

    fn single_blob_candidate() -> TxCandidate {
        TxCandidate {
            to: Some(TEST_RECIPIENT),
            blobs: Arc::from(vec![Box::<Blob>::default()]),
            ..Default::default()
        }
    }

    fn decode_eip1559(prepared: &PreparedTx) -> TxEip1559 {
        match prepared.to_envelope().expect("valid transaction envelope") {
            TxEnvelope::Eip1559(signed) => signed.strip_signature(),
            other => panic!("expected EIP-1559, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn initial_construction_produces_valid_eip1559_transaction() {
        let asserter = Asserter::new();
        push_fee_inputs(&asserter, false);
        push_gas_estimate(&asserter);
        let builder = test_builder(asserter);
        let prepared = builder
            .prepare_initial(&value_transfer(1_000_000_000), 7)
            .await
            .expect("transaction should be constructed");
        let tx = decode_eip1559(&prepared);

        assert_eq!(tx.to, TxKind::Call(TEST_RECIPIENT));
        assert_eq!(tx.value, U256::from(1_000_000_000u64));
        assert_eq!(tx.chain_id, TEST_CHAIN_ID);
        assert_eq!(tx.nonce, 7);
        assert_eq!(tx.gas_limit, 21_000);
        assert_eq!(prepared.gas_tip_cap, tx.max_priority_fee_per_gas);
        assert_eq!(prepared.gas_fee_cap, tx.max_fee_per_gas);
        assert_eq!(prepared.tx_hash, *prepared.to_envelope().unwrap().tx_hash());
    }

    #[tokio::test]
    async fn construction_preserves_gas_calldata_and_creation_kind() {
        let asserter = Asserter::new();
        push_fee_inputs(&asserter, false);
        push_gas_estimate(&asserter);
        push_fee_inputs(&asserter, false);
        push_gas_estimate(&asserter);
        let builder = test_builder(asserter);
        let calldata = Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]);
        let transfer = TxCandidate {
            to: Some(TEST_RECIPIENT),
            tx_data: calldata.clone(),
            gas_limit: 100_000,
            ..Default::default()
        };
        let prepared = builder.prepare_initial(&transfer, 3).await.unwrap();
        let tx = decode_eip1559(&prepared);
        assert_eq!(tx.input, calldata);
        assert_eq!(tx.gas_limit, 100_000);

        let creation =
            TxCandidate { to: None, tx_data: Bytes::from_static(&[0x00]), ..Default::default() };
        let created = builder.prepare_initial(&creation, 4).await.unwrap();
        assert_eq!(decode_eip1559(&created).to, TxKind::Create);
    }

    #[tokio::test]
    async fn construction_builds_cell_proof_blob_transaction() {
        let asserter = Asserter::new();
        push_fee_inputs(&asserter, true);
        push_gas_estimate(&asserter);
        let builder = test_builder(asserter);
        let prepared = builder
            .prepare_initial(&single_blob_candidate(), 0)
            .await
            .expect("blob transaction should be constructed");
        let envelope = prepared.to_envelope().expect("valid envelope");
        let signed = envelope.as_eip4844().expect("EIP-4844 envelope");
        let inner = signed.tx().tx();

        assert_eq!(inner.chain_id, TEST_CHAIN_ID);
        assert_eq!(inner.blob_versioned_hashes.len(), 1);
        assert!(inner.max_fee_per_blob_gas > 0);
        assert!(matches!(signed.tx(), TxEip4844Variant::TxEip4844WithSidecar(_)));
        assert_eq!(
            prepared.sidecar.as_ref().expect("blob sidecar").cell_proofs.len(),
            CELLS_PER_EXT_BLOB
        );
        assert_eq!(prepared.blob_fee_cap, Some(inner.max_fee_per_blob_gas));
    }

    #[tokio::test]
    async fn construction_rejects_invalid_blob_candidates() {
        let asserter = Asserter::new();
        push_fee_inputs(&asserter, true);
        push_fee_inputs(&asserter, true);
        let builder = test_builder(asserter);
        let too_many = TxCandidate {
            to: Some(TEST_RECIPIENT),
            blobs: Arc::from(
                (0..=MAX_BLOBS_PER_TX).map(|_| Box::<Blob>::default()).collect::<Vec<_>>(),
            ),
            ..Default::default()
        };
        assert!(matches!(
            builder.prepare_initial(&too_many, 0).await,
            Err(TxManagerError::Unsupported(message)) if message.contains("exceeds maximum")
        ));

        let no_recipient = TxCandidate {
            to: None,
            blobs: Arc::from(vec![Box::<Blob>::default()]),
            ..Default::default()
        };
        assert!(matches!(
            builder.prepare_initial(&no_recipient, 0).await,
            Err(TxManagerError::Unsupported(message)) if message.contains("recipient address")
        ));
    }

    #[tokio::test]
    async fn construction_applies_fee_floors() {
        let asserter = Asserter::new();
        push_fee_inputs(&asserter, false);
        push_fee_inputs(&asserter, false);
        push_gas_estimate(&asserter);
        let builder = test_builder(asserter);
        let caps = builder.suggest_gas_price_caps_for(false).await.unwrap();
        let floor = FeeOverride {
            gas_tip_cap: caps.gas_tip_cap + 50_000_000_000,
            gas_fee_cap: caps.gas_fee_cap + 100_000_000_000,
            blob_fee_cap: None,
            gas_limit_floor: 80_000,
        };
        let prepared = builder
            .prepare_with(&value_transfer(1_000), 0, Some(floor), None, None)
            .await
            .expect("fee floor should be applied");

        assert_eq!(prepared.gas_tip_cap, floor.gas_tip_cap);
        assert_eq!(prepared.gas_fee_cap, floor.gas_fee_cap);
        assert!(prepared.gas_limit >= floor.gas_limit_floor);
    }

    #[tokio::test]
    async fn fee_bump_satisfies_replacement_thresholds() {
        let asserter = Asserter::new();
        push_fee_inputs(&asserter, false);
        push_gas_estimate(&asserter);
        push_fee_inputs(&asserter, false);
        let builder = test_builder(asserter);
        let candidate = value_transfer(1_000);
        let initial = builder.prepare_initial(&candidate, 0).await.unwrap();
        let bumped = builder
            .increase_gas_price(
                &candidate,
                initial.gas_tip_cap,
                initial.gas_fee_cap,
                initial.blob_fee_cap,
            )
            .await
            .expect("fees should be bumped");

        assert!(
            bumped.gas_tip_cap >= FeeCalculator::calc_threshold_value(initial.gas_tip_cap, false)
        );
        assert!(
            bumped.gas_fee_cap >= FeeCalculator::calc_threshold_value(initial.gas_fee_cap, false)
        );
    }

    #[derive(Debug)]
    struct FailingSigner {
        address: Address,
    }

    #[async_trait]
    impl TxSigner<Signature> for FailingSigner {
        fn address(&self) -> Address {
            self.address
        }

        async fn sign_transaction(
            &self,
            _tx: &mut dyn SignableTransaction<Signature>,
        ) -> alloy_signer::Result<Signature> {
            Err(alloy_signer::Error::other("deliberately failing signer"))
        }
    }

    #[tokio::test]
    async fn signer_failure_is_propagated() {
        let asserter = Asserter::new();
        push_fee_inputs(&asserter, false);
        push_gas_estimate(&asserter);
        let builder = TxBuilder::new(
            provider_builder().connect_mocked_client(asserter),
            TokioRuntime::new(),
            EthereumWallet::from(FailingSigner { address: Address::with_last_byte(1) }),
            TxManagerConfig::default(),
            TEST_CHAIN_ID,
            Arc::new(NoopTxMetrics),
        );

        assert!(matches!(
            builder.prepare_initial(&value_transfer(1), 0).await,
            Err(TxManagerError::Sign(_))
        ));
    }
}
