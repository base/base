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

## Per-contract changed-slot limits (dry run)

The resource-metering schedule can carry an optional top-level `contractSlots` key. It counts
changed storage slots per storage owner (the account whose storage changed) in each executed
transaction's post-state, and keeps a per-block total for each owner. The block total adds each
committed transaction's count, so a slot changed by several transactions counts once per
transaction: it is an upper bound on distinct slots. A transaction is a would-limit
when its own count for one owner exceeds that owner's cap (transaction scope), or when the block
total would (block scope). Would-limits are recorded in
`base_payload.resource_metering.contract_slot_limit_exceeded_total{scope,depth}` (`depth` names
the class whose cap bound, or `flat`) and a debug log
with the owner; they never exclude, skip or evict a transaction. Only `dryRun: true` is accepted.
Sequencer and deposit transactions count toward the block totals and are never checked.

```json
{
  "version": 1,
  "dimensions": [],
  "contractSlots": {
    "dryRun": true,
    "changedSlotLimit": 2000,
    "trieDepth": {
      "nodeBudget": 2100,
      "classes": [
        { "depth": 8, "contracts": ["0x00000000000000000000000000000000000000aa"] },
        { "depth": 4, "contracts": ["0x00000000000000000000000000000000000000bb"] }
      ]
    }
  }
}
```

- `changedSlotLimit` (optional) caps every owner.
- `trieDepth` (optional) lists contracts by storage-trie depth class. At schedule load,
  `nodeBudget` becomes one cap per class: the largest number of changed slots whose expected
  number of distinct branch nodes, `U(k, d) = Σ_{l<d} 16^l · (1 − (1 − 16^−l)^k)`, fits the
  budget. A budget of 2,100 gives caps of 2,419 (depth 4), 973 (5), 632 (6), 473 (7) and
  380 (8); depth 3 and shallower have no cap. A listed contract's cap is the smaller of its class
  cap and `changedSlotLimit`.
- At least one of the two is required.

`etc/scripts/local/metering-depth-table.py` generates the `trieDepth` object over RPC and
documents the refresh procedure. The schedule is read once at startup, and older binaries reject
the `contractSlots` key, so a binary with this support must be deployed before any schedule uses it.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
