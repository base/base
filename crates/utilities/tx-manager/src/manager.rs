//! Public façade: construct [`SimpleTxManager`] and submit transactions.

mod build;
pub use build::{PreparedTx, TxBuilder, WEI_PER_GWEI};

mod coordinator;
pub use coordinator::{
    CoordinatorCommand, CoordinatorHandle, CoordinatorWorkers, TxCoordinator, WorkerEvent,
};

mod pending;
pub use pending::{
    CancelRequest, NonceFetch, NonceSlot, PendingLedger, PendingPolicy, PendingWork,
    PublishedAttempt, RejectionVerdict, ReplacementReason, ReplacementState, SignedVersion,
    SlotEffects, SlotState, StagedSubmission, VersionId, VersionKind,
};

mod publisher;
pub use publisher::{
    AcceptedPosition, AttemptedPosition, PublishOutcome, PublishReject, PublisherCursor,
    PublisherEvent, PublisherGroup, PublisherId, PublisherSnapshot, PublisherTx, TxPublisher,
};

mod sweep;
pub use sweep::{
    ChainSweeper, MAX_CONCURRENT_SWEEP_QUERIES, SUPERSESSION_OBSERVATIONS, SupersessionEvidence,
    SweepOutcome, SweepResolution, SweepTarget,
};

use std::{fmt::Debug, future::Future, sync::Arc};

use alloy_network::{Ethereum, EthereumWallet, NetworkWallet};
use alloy_primitives::Address;
use alloy_provider::Provider;
use base_runtime::{Runtime, RuntimeTimeout, TokioRuntime};

use crate::{
    SignerConfig, SubmissionHandle, TxCandidate, TxManagerConfig, TxManagerError, TxManagerResult,
    TxMetrics, error::RpcErrorClassifier,
};

/// Interface for submitting and managing transactions.
///
/// [`Self::submit`] returns immediately. Callers observe or await the returned
/// [`SubmissionHandle`] while the manager prepares, publishes, and confirms the
/// transaction in the background.
pub trait TxManager: Send + Sync + Debug {
    /// Enqueues a transaction and returns its lifecycle handle.
    fn submit(&self, candidate: TxCandidate) -> SubmissionHandle;

    /// Returns the address transactions are sent from.
    fn sender_address(&self) -> Address;

    /// Attempts to clear the oldest stuck nonce with a higher-fee self-transfer.
    ///
    /// Success means the cancellation transaction may be live. Confirmation
    /// can still be pending.
    fn cancel_tx(&self) -> impl Future<Output = TxManagerResult<()>> + Send {
        std::future::ready(Ok(()))
    }
}

/// Default transaction-manager implementation.
///
/// Cheap to clone: every clone talks to the same background coordinator
/// and therefore shares one nonce space.
#[derive(Debug, Clone)]
pub struct SimpleTxManager {
    /// Address this manager signs with.
    sender: Address,
    /// Handle used to submit work to the background coordinator.
    coordinator: CoordinatorHandle,
}

impl SimpleTxManager {
    /// Creates a manager that publishes only through `provider`.
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

    /// Creates a manager with a custom runtime and extra publish backends.
    ///
    /// `chain_provider` is used to read chain state (nonce, receipts) and is
    /// also the first publish backend. `additional_publishers` are extra
    /// backends that receive the same signed transactions.
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

    /// Validates setup, then starts the background coordinator.
    ///
    /// Call this only with a ready wallet. Prefer [`Self::new`] or
    /// [`Self::new_with_runtime_and_publishers`] from application code.
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
        // Phase 1: validate config locally. Fail before any RPC.
        config.validate().map_err(|error| TxManagerError::InvalidConfig(error.to_string()))?;

        // Phase 2: check that every RPC backend is on the expected chain.
        // `chain_provider` and each extra publisher must all return `chain_id`.
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

        // Phase 3: read the account's next nonce from the chain.
        // This is the starting value for `PendingLedger`; it only increases after this.
        let sender = <EthereumWallet as NetworkWallet<Ethereum>>::default_signer_address(&wallet);
        let next_nonce = RuntimeTimeout::run(
            &runtime,
            config.network_timeout,
            chain_provider.get_transaction_count(sender).latest(),
        )
        .await
        .map_err(|_| TxManagerError::Rpc("initial nonce query timed out".to_string()))?
        .map_err(|error| RpcErrorClassifier::classify_rpc_error(&error))?;

        // Phase 4: construct workers and the ledger. Nothing is spawned yet.
        // - TxBuilder: estimates gas, signs the tx, rebuilds it on fee bump / cancel
        // - ChainSweeper: reads the chain and reports which pending nonces are confirmed
        // - PublisherGroup: sends each signed tx to every RPC backend, in nonce order
        // - PendingLedger: in-memory queue of staged and pending txs
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
            Arc::clone(&metrics),
        );

        let ledger = PendingLedger::new(
            next_nonce,
            publishers.len(),
            PendingPolicy {
                publish_retry_delay: config.publish_retry_delay,
                resubmission_timeout: config.resubmission_timeout,
                tx_not_in_mempool_timeout: (!config.tx_not_in_mempool_timeout.is_zero())
                    .then_some(config.tx_not_in_mempool_timeout),
            },
        );

        // Phase 5: start the coordinator task.
        // Config, chain-id, and nonce checks already succeeded above.
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
