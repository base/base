# `base-node-service`

Base execution startup and lifecycle shared by `base rpc` and `base sequencer`.

`NodeLaunch` owns the resolved configuration, database, and runtime. It constructs the Base
pool, network, consensus, payload service, and RPC APIs directly. Consensus receives the
execution driver and local provider from the returned `NodeHandle`.

`BasePayloadServiceConfig` selects the standard dedicated-thread service or the sequencer's
full-block deadline. `BaseRpcServices` and `NodeServices` carry runtime settings for the built-in
transaction, bundle, metering, proof-history, forwarding, tracing, and upgrade-signal services.
HTTP/WS transport settings and service enablement remain operational options.

```rust,ignore
use base_node_service::NodeLaunch;

let node = NodeLaunch::new(config, database, task_executor).launch().await?;
```
