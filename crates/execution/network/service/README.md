# Execution networking service

Base execution-layer peer discovery, sessions, request serving, and transaction gossip.

Ethereum's networking protocol is specified in [devp2p](https://github.com/ethereum/devp2p).

In order for a node to join the ethereum p2p network it needs to know what nodes are already
part of that network. This includes public identities (public key) and addresses (where to reach
them).

## Bird's Eye View

See also diagram in [`NetworkManager`]

The `Network` is made up of several, separate tasks:

   - `Transactions Task`: is a spawned
     [`TransactionsManager`](crate::transactions::TransactionsManager) future that:

       * Responds to incoming transaction related requests
       * Requests missing transactions from the `Network`
       * Broadcasts new transactions received from the
         [`TransactionPool`](base_execution_txpool::TransactionPool) over the `Network`

   - `ETH request Task`: is a spawned
     [`EthRequestHandler`](crate::eth_requests::EthRequestHandler) future that:

       * Responds to incoming ETH related requests: `Headers`, `Bodies`

   - `Discovery Task`: is a spawned [`Discv4`](base_execution_network_discovery::Discv4) future that handles peer
     discovery and emits new peers to the `Network`

   - [`NetworkManager`] task advances the state of the `Network`, which includes:

       * Initiating new _outgoing_ connections to discovered peers
       * Handling _incoming_ TCP connections from peers
       * Peer management
       * Route requests:
            - from remote peers to corresponding tasks
            - from local to remote peers

## Usage

### Configure and launch a standalone network

The [`NetworkConfig`] is used to configure the network.
A block reader is attached separately when the network starts.

```
# async fn launch() {
use base_execution_network_service::{
    config::rng_secret_key, NetworkConfig, NetworkManager,
};
use base_execution_network_wire::mainnet_nodes;
use base_execution_state_database::NoopProvider;
use base_common_runtime::Runtime;

// This block provider implementation is used for testing purposes.
let client = NoopProvider::default();

// The key that's used for encrypting sessions and to identify our node.
let local_key = rng_secret_key();

let config = NetworkConfig::builder(local_key, Runtime::test())
    .boot_nodes(mainnet_nodes())
    .build(client.clone());

// create the network instance
let network = NetworkManager::new(config, client).await.unwrap();

// keep a handle to the network and spawn it
let handle = network.handle().clone();
tokio::task::spawn(network);

# }
```

### Configure all components of the Network with the [`NetworkBuilder`]

```rust
use base_execution_network_service::{NetworkConfig, NetworkManager};
use base_execution_state_provider::BlockchainProvider;
use base_execution_txpool::BaseTransactionPool;
use base_common_runtime::Runtime;

async fn launch(client: BlockchainProvider, pool: BaseTransactionPool) {
    let config = NetworkConfig::builder_with_rng_secret_key(Runtime::test())
        .build(client.clone());
    let transactions_config = config.transactions_manager_config.clone();
    let (handle, network, transactions, request_handler) =
        NetworkManager::builder(config, client.clone()).await.unwrap()
            .transactions(pool, transactions_config)
            .request_handler(client)
            .split_with_handle();
}
```

# Feature Flags

- `serde`: Enable serde support for configuration types.
- `test-utils`: Various utilities helpful for writing tests

