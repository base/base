# `base-common-rpc-types-engine`

Base chain RPC types for the `engine` namespace.

## Overview

Defines execution engine payload types for the consensus-to-execution Engine API. Includes
`OpPayloadAttributes` for block building requests, versioned payload envelopes
(`OpExecutionPayloadEnvelope`, `OpNetworkPayloadEnvelope`), `OpExecutionPayloadV4`, and
versioned sidecars (`OpExecutionPayloadSidecar`). These types are exchanged between the
consensus client and the execution node via the Engine API.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-common-rpc-types-engine = { workspace = true }
```

```rust,ignore
use base_common_rpc_types_engine::{OpPayloadAttributes, OpExecutionPayloadEnvelope};

let attrs = OpPayloadAttributes { timestamp, transactions, .. };
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
