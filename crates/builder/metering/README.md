# `base-builder-metering`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Resource metering backend for the Base block builder. Provides a concrete [`MeteringProvider`](../core/) implementation backed by a concurrent cache with LRU eviction, along with JSON-RPC extensions for managing resource metering data and the builder's resource-throttle schedule.

## Overview

- **`MeteringStore`**: Thread-safe metering data cache using `DashMap` with bounded LRU eviction. Implements `MeteringProvider` from `base-builder-core`.
- **`MeteringStoreExt`**: JSON-RPC extension exposing `base_setMeteringInformation`, `base_setMeteringEnabled`, and `base_clearMeteringInformation` methods.
- **`ResourceThrottleExt`**: JWT-authenticated JSON-RPC extension exposing
  `base_getResourceThrottleSchedule` and `base_replaceResourceThrottleSchedule`.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-builder-metering = { git = "https://github.com/base/base" }
```

Create a store and wire it into the builder config:

```rust,ignore
use std::sync::Arc;
use base_builder_metering::{MeteringStore, MeteringStoreExt};
use std::time::Duration;

let store = Arc::new(MeteringStore::new(true, 10_000, Duration::from_secs(30)));
// Pass `store.clone()` as `SharedMeteringProvider` into `BuilderConfig`
// Pass `store` into `MeteringStoreExt::new()` for the RPC extension
```

## Resource-throttle schedules

Resource throttling is builder-local accounting. It does not change protocol gas prices, transaction
fees, or the gas limit accepted by the EVM. A schedule converts the gas and selectively collected
`opcodeGas` entries from `meterBundle` into bounded resource units.

Configure a startup schedule with:

```text
--builder.resource-throttle-schedule=/etc/base/resource-throttle.json
```

The same path can be supplied through `BUILDER_RESOURCE_THROTTLE_SCHEDULE`.

The schedule is versioned and contains block-scoped dimensions. `baseGasWeight` charges against
actual transaction gas used; operation rules can additionally charge measured gas and/or counts.
The same operation may be priced in multiple dimensions.

```json
{
  "version": 1,
  "dimensions": [
    {
      "name": "execution",
      "blockLimit": 100000000,
      "transactionLimit": 10000000,
      "baseGasWeight": 1,
      "operations": [
        { "name": "SSTORE", "gasUsedWeight": 2, "countCost": 100 }
      ]
    },
    {
      "name": "proof",
      "blockLimit": 50000000,
      "baseGasWeight": 0,
      "operations": [
        { "name": "BLAKE2F", "gasUsedWeight": 4, "countCost": 1000 },
        { "name": "TX_EFFECT_ETH_TRANSFER_TO_NONEXISTENT_ACCOUNT", "countCost": 25000 }
      ]
    }
  ]
}
```

The additive calculation for each dimension is:

```text
baseGasWeight * gasUsed
  + sum(gasUsedWeight * opcodeGas.gasUsed + countCost * opcodeGas.count)
```

Resource-throttle schedules are snapshotted when a block build starts. A runtime replacement
therefore takes effect on the next payload build and never changes the rules for an in-flight
block. Both replacement methods are registered only on the JWT-authenticated RPC endpoint:

```text
base_getResourceThrottleSchedule()
base_replaceResourceThrottleSchedule(schedule, expectedRevision?)
```

`expectedRevision` provides compare-and-swap behavior for incident automation. The replacement is
validated atomically; invalid schedules and stale revisions are rejected. The resource-throttle
schedule is evaluated only when builder metering is enabled and supports `off`, `dry-run`, and
`enforce` rollout behavior through `--builder.execution-metering-mode`.
The legacy execution-time and state-root-gas limits remain available during rollout; if configured,
they are evaluated alongside resource-throttle budgets.

For example, send these JSON-RPC requests to the authenticated endpoint with the node's JWT:

```json
{"jsonrpc":"2.0","id":1,"method":"base_getResourceThrottleSchedule","params":[]}
{"jsonrpc":"2.0","id":2,"method":"base_replaceResourceThrottleSchedule","params":[
  {"version":1,"dimensions":[]},
  7
]}
```

The replacement response is the new revision. Pass `null` as the second parameter to replace
unconditionally; using the revision returned by `base_getResourceThrottleSchedule` is safer for
incident automation.

The schedule can only price data that the builder receives. `meterBundle` nodes must be configured
with the union of opcode, precompile, and pseudo-opcode names required by the schedule and
restarted when that collection set changes. An absent `opcodeGas` entry means the operation was not
observed by the metering node; it is not evidence that the operation is impossible.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
