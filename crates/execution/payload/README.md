# `base-execution-payload-builder`

Payload builder for Base.

## Overview

Implements Base payload building and validation for the Base execution node. The
`BasePayloadBuilder` assembles new execution payloads from transaction pool contents and
`BasePayloadBuilderAttributes` received from the consensus layer. `BaseExecutionPayloadValidator`
verifies
built payloads against consensus rules. Also provides data availability configuration via
`BaseDAConfig` for fee calculation.

Resource metering is an optional payload-builder admission guardrail. A
file-backed schedule prices named observations into independent resource-unit
dimensions. Simulated `meterBundle` data is used only to skip candidates that
are predicted to exceed a remaining budget. Committed usage is accounted from
executed results when they exist: actual gas used and net post-state effects
(`STATE_NEW_STORAGE_SLOT`, `STATE_CHANGED_STORAGE_SLOT`,
`STATE_CLEARED_STORAGE_SLOT`, `STATE_TOUCHED_ACCOUNT`, and
`STATE_CHANGED_ACCOUNT` for balance, nonce, or code changes) replace simulated
`STATE_*` rows. Other simulated opcode and precompile rows are kept;
production execution does not attach opcode bags. Storage-only writes are
counted by the `STATE_*_STORAGE_SLOT` operations, not `STATE_CHANGED_ACCOUNT`.
Omitted `transactionLimit` compiles to that dimension’s `blockLimit`. Resource
metering then excludes transactions whose accounted usage exceeds an enforced
budget. A dimension may set
`dryRun` to observe that budget without excluding. Block-scope skips apply
only to the current payload scan; the transaction stays in the pool for a
later block. Transaction-scope skips (predicted or executed) are permanent
pool evictions and are recorded in a shared TTL rejection cache (30
minutes), matching Flashblocks: later payload jobs skip that hash even if
the transaction is re-gossiped into the pool. Nonce-lane descendants are
skipped for the current scan; skipping those descendants across later jobs
is Flashblocks-only. This does not change protocol gas, fees, or validity.

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
