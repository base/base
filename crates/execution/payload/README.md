# `base-execution-payload-builder`

Payload builder for Base.

## Overview

Implements Base payload building and validation for the Base execution node. The
`BasePayloadBuilder` assembles new execution payloads from transaction pool contents and
`BasePayloadBuilderAttributes` received from the consensus layer. `BaseExecutionPayloadValidator`
verifies
built payloads against consensus rules. Also provides data availability configuration via
`BaseDAConfig` for fee calculation.

## Validity metadata

`BaseBuilderConfig::validity_metadata` holds a
[`ValidityMetadataRecorder`](../validity-metadata/README.md), disabled by default. When enabled,
the builder captures the hash of each pool transaction that carried nonempty validity predicates
once its execution commits, including EVM reverts. Deferred, expired and rejected candidates,
empty or signature-only sidecars, and attribute transactions are never captured. After the block
is sealed, a pool build (`no_tx_pool` unset) hands one record with the sealed block hash, number,
total transaction count and captured hashes to the recorder, even when no validity transaction was
included. The hand-off never blocks and performs no I/O; derived-attribute builds, witness
generation and cancelled or aborted builds record nothing. Recording never changes transaction
selection, ordering, fees or the built block.

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

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
