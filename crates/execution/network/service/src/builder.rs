//! Builder support for configuring the entire setup.

use base_common_observability_metrics::common::mpsc::memory_bounded_channel;
use tokio::sync::mpsc;

use crate::{
    NetworkHandle, NetworkManager, PeersHandleProvider,
    eth_requests::EthRequestHandler,
    metrics::NETWORK_POOL_TRANSACTIONS_SCOPE,
    transactions::{
        TransactionsManager, TransactionsManagerConfig, config::TransactionPropagationKind,
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

    /// Creates a new [`TransactionsManager`] and wires it to the network.
    pub fn transactions(
        self,
        pool: base_execution_txpool::BaseTransactionPool,
        transactions_manager_config: TransactionsManagerConfig,
    ) -> NetworkBuilder<TransactionsManager, Eth> {
        self.transactions_with_policy(
            pool,
            transactions_manager_config,
            TransactionPropagationKind::default(),
        )
    }

    /// Creates a transaction manager with the selected propagation policy and strict Base filtering.
    pub fn transactions_with_policy(
        self,
        pool: base_execution_txpool::BaseTransactionPool,
        transactions_manager_config: TransactionsManagerConfig,
        propagation_policy: TransactionPropagationKind,
    ) -> NetworkBuilder<TransactionsManager, Eth> {
        let Self { mut network, request_handler, .. } = self;
        let (tx, rx) = memory_bounded_channel(
            transactions_manager_config.tx_channel_memory_limit_bytes,
            NETWORK_POOL_TRANSACTIONS_SCOPE,
            base_common_types_chain::InMemorySize::size,
        );
        network.set_transactions(tx);
        let handle = network.handle().clone();

        let transactions = TransactionsManager::with_policy(
            handle,
            pool,
            rx,
            transactions_manager_config,
            propagation_policy,
        );
        NetworkBuilder { network, request_handler, transactions }
    }
}
