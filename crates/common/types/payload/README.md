# `base-common-types-payload`

Shared execution payload and fork-choice types.

## Overview

Defines the payloads, attributes, statuses, fork-choice state, sidecars, and gossip envelopes used
by Base consensus and the execution driver. `ExecutionData` combines Base payloads with their
sidecars. These types support direct driver calls; this crate does not provide an Engine RPC
client or server. JWT helpers remain available for authenticated RPC consumers.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-common-types-payload = { workspace = true }
```

```rust,ignore
use base_common_types_payload::{BaseExecutionPayloadEnvelope, BasePayloadAttributes};

let attrs: BasePayloadAttributes = todo!();
let envelope: BaseExecutionPayloadEnvelope = todo!();
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
