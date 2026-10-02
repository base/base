# `base-execution-payload-builder`

Payload builder for Base.

## Overview

Implements Base payload building and validation for the Base execution node. The
`BasePayloadBuilder` assembles new execution payloads from transaction pool contents and
`BasePayloadBuilderAttributes` received from the consensus layer. `BaseExecutionPayloadValidator`
verifies
built payloads against consensus rules. Also provides data availability configuration via
`BaseDAConfig` for fee calculation.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-payload-builder = { workspace = true }
```

```rust,ignore
use base_execution_payload_builder::BasePayloadBuilder;

let builder = BasePayloadBuilder::new(evm_config, payload_validator);
let payload = builder.build_payload(attrs, best_payload)?;
```

## Predicate-state prewarming (opt-in)

With `--builder.enable-prewarming`, flashblocks builds warm the declared
validity-predicate state (balances and storage slots) of bounded lookahead transactions
into the shared execution cache on a fixed pool of IO threads, so the build loop can
serve upcoming reads from memory. Prewarming is read-only and does not
change inclusion or parking decisions. Scheduling uses short mutex-protected sections
and never waits for IO. Cancellation discards queued work; each worker releases its cache after any in-flight read or provider-open operation finishes.
The flag auto-enables `--engine.share-execution-cache-with-payload-builder` in both
binaries. Without a shared cache, prewarming is skipped.

The flashblocks builder runs 2 IO workers, scans 1024 lookahead transactions per block
build, and warms at most 4096 distinct keys per build. Busy workers skip new builds
rather than queueing old-parent work. The pool is additional to engine prewarming.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
