//! Transaction-manager façade backed by one ordered lifecycle coordinator.

mod build;
pub use build::{PreparedTx, TxBuilder, WEI_PER_GWEI};

mod coordinator;
pub use coordinator::{
    CoordinatorCommand, CoordinatorHandle, CoordinatorWorkers, TxCoordinator, WorkerEvent,
};

mod pending;
pub use pending::{
    AdmissionBudget, PendingAdmission, PendingLedger, PendingPolicy, PendingSlot, PendingWork,
    PublishedAttempt, ReplacementReason, ReplacementRequest, SignedVersion, SlotState,
    StagedSubmission, SubmissionCompletion, SubmissionTracker, SweepOutcome, SweepResolution,
    SweepTarget, VersionId, VersionKind,
};

mod publisher;
pub use publisher::{
    AcceptedPosition, AttemptedPosition, PublishOutcome, PublishReject, PublisherCursor,
    PublisherEvent, PublisherGroup, PublisherId, PublisherSnapshot, PublisherTx, TxPublisher,
};

mod sweep;
pub use sweep::{
    ChainSweeper, MAX_CONCURRENT_SWEEP_QUERIES, SUPERSESSION_OBSERVATIONS, SupersessionEvidence,
};

use std::{fmt::Debug, sync::Arc};

use alloy_network::{Ethereum, EthereumWallet, NetworkWallet};
use alloy_primitives::Address;
use alloy_provider::Provider;
use base_runtime::{Runtime, RuntimeTimeout, TokioRuntime};

use crate::{
    SignerConfig, SubmissionHandle, TxCandidate, TxManager, TxManagerConfig, TxManagerError,
    TxManagerResult, TxMetrics, error::RpcErrorClassifier,
};

/// Default transaction-manager implementation.
///
/// Clones share one coordinator and therefore one ordered nonce space.
#[derive(Debug, Clone)]
pub struct SimpleTxManager {
    /// Address used to sign every managed transaction.
    sender: Address,
    /// Cloneable command boundary for the background lifecycle coordinator.
    coordinator: CoordinatorHandle,
}

impl SimpleTxManager {
    /// Creates a manager whose chain provider is also its sole publisher.
    pub async fn new<P>(
        provider: P,
        signer_config: SignerConfig,
        config: TxManagerConfig,
        chain_id: u64,
        metrics: Arc<dyn TxMetrics>,
    ) -> TxManagerResult<Self>
    where
        P: Provider + Clone + Debug + Send + Sync + 'static,
    {
        Self::new_with_runtime_and_publishers(
            TokioRuntime::new(),
            provider,
            Vec::new(),
            signer_config,
            config,
            chain_id,
            metrics,
        )
        .await
    }

    /// Creates a manager with an injected runtime and additional publication backends.
    pub async fn new_with_runtime_and_publishers<P, R>(
        runtime: R,
        chain_provider: P,
        additional_publishers: Vec<P>,
        signer_config: SignerConfig,
        config: TxManagerConfig,
        chain_id: u64,
        metrics: Arc<dyn TxMetrics>,
    ) -> TxManagerResult<Self>
    where
        P: Provider + Clone + Debug + Send + Sync + 'static,
        R: Runtime,
    {
        let wallet = signer_config.build_wallet()?;
        Self::start(
            runtime,
            chain_provider,
            additional_publishers,
            wallet,
            config,
            chain_id,
            metrics,
        )
        .await
    }

    /// Creates a fully configured manager and starts its coordinator task.
    ///
    /// Construction validates configuration and every provider's chain ID,
    /// reads the initial canonical account nonce, wires stateless workers,
    /// and finally transfers lifecycle ownership to one coordinator task.
    pub async fn start<P, R>(
        runtime: R,
        chain_provider: P,
        additional_publishers: Vec<P>,
        wallet: EthereumWallet,
        config: TxManagerConfig,
        chain_id: u64,
        metrics: Arc<dyn TxMetrics>,
    ) -> TxManagerResult<Self>
    where
        P: Provider + Clone + Debug + Send + Sync + 'static,
        R: Runtime,
    {
        // Phase 1: reject invalid local policy before issuing startup RPCs.
        config.validate().map_err(|error| TxManagerError::InvalidConfig(error.to_string()))?;

        // Phase 2: all destinations must identify the same chain. No publisher
        // is authoritative, so every backend must pass the same startup check.
        let chain_provider_chain_id =
            RuntimeTimeout::run(&runtime, config.network_timeout, chain_provider.get_chain_id())
                .await
                .map_err(|_| TxManagerError::Rpc("get_chain_id timed out".to_string()))?
                .map_err(|error| RpcErrorClassifier::classify_rpc_error(&error))?;
        if chain_provider_chain_id != chain_id {
            return Err(TxManagerError::InvalidConfig(format!(
                "chain_id mismatch: supplied {chain_id}, provider returned {chain_provider_chain_id}"
            )));
        }
        for (index, publisher) in additional_publishers.iter().enumerate() {
            let backend = index.saturating_add(1);
            let publisher_chain_id =
                RuntimeTimeout::run(&runtime, config.network_timeout, publisher.get_chain_id())
                    .await
                    .map_err(|_| {
                        TxManagerError::Rpc(format!(
                            "publisher backend {backend} chain ID query timed out"
                        ))
                    })?
                    .map_err(|error| RpcErrorClassifier::classify_rpc_error(&error))?;
            if publisher_chain_id != chain_id {
                return Err(TxManagerError::InvalidConfig(format!(
                    "publisher backend {backend} chain_id mismatch: supplied {chain_id}, provider returned {publisher_chain_id}"
                )));
            }
        }

        // Phase 3: seed the sole nonce authority from the chain reader.
        // PendingLedger advances this value monotonically after construction.
        let sender = <EthereumWallet as NetworkWallet<Ethereum>>::default_signer_address(&wallet);
        let next_nonce = RuntimeTimeout::run(
            &runtime,
            config.network_timeout,
            chain_provider.get_transaction_count(sender).latest(),
        )
        .await
        .map_err(|_| TxManagerError::Rpc("initial nonce query timed out".to_string()))?
        .map_err(|error| RpcErrorClassifier::classify_rpc_error(&error))?;

        // Phase 4: workers own network operations; only PendingLedger, inside the
        // coordinator, owns mutable transaction lifecycle state.
        let builder = TxBuilder::new(
            chain_provider.clone(),
            runtime.clone(),
            wallet,
            config.clone(),
            chain_id,
            Arc::clone(&metrics),
        );
        let sweeper = ChainSweeper::new(
            chain_provider.clone(),
            runtime.clone(),
            sender,
            config.num_confirmations,
            config.network_timeout,
            Arc::clone(&metrics),
        );
        let mut publication_backends =
            Vec::with_capacity(additional_publishers.len().saturating_add(1));
        publication_backends.push(chain_provider);
        publication_backends.extend(additional_publishers);
        let (publishers, publisher_events) = PublisherGroup::new(
            publication_backends,
            runtime.clone(),
            config.network_timeout,
            config.publish_retry_delay,
            Arc::clone(&metrics),
        );
        let ledger = PendingLedger::new(
            next_nonce,
            publishers.len(),
            PendingPolicy {
                publish_max_retries: config.publish_max_retries,
                publish_retry_delay: config.publish_retry_delay,
                resubmission_timeout: config.resubmission_timeout,
                tx_not_in_mempool_timeout: config.tx_not_in_mempool_timeout,
            },
        );

        // Phase 5: spawn only after every fallible startup check succeeds.
        let (coordinator, coordinator_handle) = TxCoordinator::new(
            ledger,
            CoordinatorWorkers { builder, sweeper, publishers },
            publisher_events,
            sender,
            runtime.clone(),
            config,
            metrics,
        );
        runtime.spawn(coordinator.run());

        Ok(Self { sender, coordinator: coordinator_handle })
    }
}

impl TxManager for SimpleTxManager {
    fn submit(&self, candidate: TxCandidate) -> SubmissionHandle {
        self.coordinator.submit(candidate)
    }

    fn sender_address(&self) -> alloy_primitives::Address {
        self.sender
    }

    async fn cancel_tx(&self) -> TxManagerResult<()> {
        self.coordinator.cancel().await
    }
}

#[cfg(test)]
mod tests {
    use alloy_provider::{builder as provider_builder, mock::Asserter};
    use alloy_signer_local::PrivateKeySigner;

    use super::*;
    use crate::NoopTxMetrics;

    #[tokio::test]
    async fn constructor_rejects_chain_id_mismatch() {
        let asserter = Asserter::new();
        asserter.push_success(&"0x1");
        let provider = provider_builder().connect_mocked_client(asserter);
        let signer = PrivateKeySigner::from_slice(&[1_u8; 32]).unwrap();

        let error = SimpleTxManager::new(
            provider,
            SignerConfig::local(signer),
            TxManagerConfig::default(),
            2,
            Arc::new(NoopTxMetrics),
        )
        .await
        .unwrap_err();

        assert!(
            matches!(error, TxManagerError::InvalidConfig(message) if message.contains("chain_id"))
        );
    }

    #[tokio::test]
    async fn constructor_validates_configuration_before_network_startup() {
        let provider = provider_builder().connect_mocked_client(Asserter::new());
        let signer = PrivateKeySigner::from_slice(&[1_u8; 32]).unwrap();

        let error = SimpleTxManager::new(
            provider,
            SignerConfig::local(signer),
            TxManagerConfig { num_confirmations: 0, ..TxManagerConfig::default() },
            1,
            Arc::new(NoopTxMetrics),
        )
        .await
        .unwrap_err();

        assert!(
            matches!(error, TxManagerError::InvalidConfig(message) if message.contains("num_confirmations"))
        );
    }

    #[tokio::test]
    async fn constructor_rejects_additional_publisher_chain_id_mismatch() {
        let chain_reader = Asserter::new();
        chain_reader.push_success(&"0x1");
        let publisher = Asserter::new();
        publisher.push_success(&"0x2");
        let signer = PrivateKeySigner::from_slice(&[1_u8; 32]).unwrap();

        let error = SimpleTxManager::new_with_runtime_and_publishers(
            TokioRuntime::new(),
            provider_builder().connect_mocked_client(chain_reader),
            vec![provider_builder().connect_mocked_client(publisher)],
            SignerConfig::local(signer),
            TxManagerConfig::default(),
            1,
            Arc::new(NoopTxMetrics),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, TxManagerError::InvalidConfig(message)
                if message.contains("publisher backend 1")
                    && message.contains("chain_id mismatch")));
    }
}
