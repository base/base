//! Builder support for configuring the entire setup.

use base_common_observability_metrics::common::mpsc::memory_bounded_channel;
use base_execution_state_types::BalProvider;
use base_execution_txpool::{BlobStore, TransactionPool};
use tokio::sync::mpsc;

use crate::{
    NetworkHandle, NetworkManager, PeersHandleProvider,
    eth_requests::EthRequestHandler,
    metrics::NETWORK_POOL_TRANSACTIONS_SCOPE,
    transactions::{
        TransactionPropagationPolicy, TransactionsManager, TransactionsManagerConfig,
        config::{
            AnnouncementFilteringPolicy, StrictEthAnnouncementFilter, TransactionPropagationKind,
        },
        policy::NetworkPolicies,
    },
};

/// We set the max channel capacity of the `EthRequestHandler` to 256
/// 256 requests with malicious 10MB body requests is 2.6GB which can be absorbed by the node.
pub(crate) const ETH_REQUEST_CHANNEL_CAPACITY: usize = 256;

/// A builder that can configure all components of the network.
#[expect(missing_debug_implementations)]
pub struct NetworkBuilder<Tx, Eth> {
    pub(crate) network: NetworkManager,
    pub(crate) transactions: Tx,
    pub(crate) request_handler: Eth,
}

// === impl NetworkBuilder ===

impl<Tx, Eth> NetworkBuilder<Tx, Eth> {
    /// Maps the transactions component.
    pub fn map_transactions<F, NewTx>(self, f: F) -> NetworkBuilder<NewTx, Eth>
    where
        F: FnOnce(Tx) -> NewTx,
    {
        let Self { network, transactions, request_handler } = self;
        NetworkBuilder { network, transactions: f(transactions), request_handler }
    }

    /// Consumes the type and returns all fields.
    pub fn split(self) -> (NetworkManager, Tx, Eth) {
        let Self { network, transactions, request_handler } = self;
        (network, transactions, request_handler)
    }

    /// Returns the network manager.
    pub const fn network(&self) -> &NetworkManager {
        &self.network
    }

    /// Returns the mutable network manager.
    pub const fn network_mut(&mut self) -> &mut NetworkManager {
        &mut self.network
    }

    /// Returns the handle to the network.
    pub fn handle(&self) -> NetworkHandle {
        self.network.handle().clone()
    }

    /// Consumes the type and returns all fields and also return a [`NetworkHandle`].
    pub fn split_with_handle(self) -> (NetworkHandle, NetworkManager, Tx, Eth) {
        let Self { network, transactions, request_handler } = self;
        let handle = network.handle().clone();
        (handle, network, transactions, request_handler)
    }

    /// Creates a new [`EthRequestHandler`] and wires it to the network.
    pub fn request_handler(
        self,
        client: base_execution_state_provider::BlockchainProvider,
    ) -> NetworkBuilder<Tx, EthRequestHandler> {
        let Self { mut network, transactions, .. } = self;
        let (tx, rx) = mpsc::channel(ETH_REQUEST_CHANNEL_CAPACITY);
        network.set_eth_request_handler(tx);
        let peers = network.handle().peers_handle().clone();
        let request_handler = EthRequestHandler::new(client, peers, rx);
        NetworkBuilder { network, request_handler, transactions }
    }

    /// Creates a new [`EthRequestHandler`] with access to a blob store and wires it to the network.
    pub fn request_handler_with_blob_store(
        self,
        client: base_execution_state_provider::BlockchainProvider,
        blob_store: Box<dyn BlobStore>,
    ) -> NetworkBuilder<Tx, EthRequestHandler> {
        let NetworkBuilder { network, transactions, request_handler } =
            self.request_handler(client);
        let request_handler = request_handler.with_blob_store(blob_store);
        NetworkBuilder { network, request_handler, transactions }
    }

    /// Creates a new [`TransactionsManager`] and wires it to the network.
    pub fn transactions<S: base_execution_txpool::BlobStore + Clone>(
        self,
        pool: base_execution_txpool::BaseTransactionPool<S>,
        transactions_manager_config: TransactionsManagerConfig,
    ) -> NetworkBuilder<TransactionsManager<S>, Eth> {
        self.transactions_with_policy(
            pool,
            transactions_manager_config,
            TransactionPropagationKind::default(),
        )
    }

    /// Creates a new [`TransactionsManager`] and wires it to the network.
    ///
    /// Uses the default [`StrictEthAnnouncementFilter`] for announcement filtering.
    pub fn transactions_with_policy<
        S: base_execution_txpool::BlobStore + Clone,
        P: TransactionPropagationPolicy,
    >(
        self,
        pool: base_execution_txpool::BaseTransactionPool<S>,
        transactions_manager_config: TransactionsManagerConfig,
        propagation_policy: P,
    ) -> NetworkBuilder<TransactionsManager<S>, Eth> {
        self.transactions_with_policies(
            pool,
            transactions_manager_config,
            propagation_policy,
            StrictEthAnnouncementFilter::default(),
        )
    }

    /// Creates a new [`TransactionsManager`] with custom propagation and announcement policies.
    ///
    /// This allows chains with custom transaction types (like CATX) to configure
    /// the announcement filter to accept their transaction types.
    pub fn transactions_with_policies<
        S: base_execution_txpool::BlobStore + Clone,
        P: TransactionPropagationPolicy,
        A: AnnouncementFilteringPolicy,
    >(
        self,
        pool: base_execution_txpool::BaseTransactionPool<S>,
        transactions_manager_config: TransactionsManagerConfig,
        propagation_policy: P,
        announcement_policy: A,
    ) -> NetworkBuilder<TransactionsManager<S>, Eth> {
        let Self { mut network, request_handler, .. } = self;
        let (tx, rx) = memory_bounded_channel(
            transactions_manager_config.tx_channel_memory_limit_bytes,
            NETWORK_POOL_TRANSACTIONS_SCOPE,
            base_common_types_chain::InMemorySize::size,
        );
        network.set_transactions(tx);
        let handle = network.handle().clone();
        let policies = NetworkPolicies::new(propagation_policy, announcement_policy);

        let transactions = TransactionsManager::with_policy(
            handle,
            pool,
            rx,
            transactions_manager_config,
            policies,
        );
        NetworkBuilder { network, request_handler, transactions }
    }
}
