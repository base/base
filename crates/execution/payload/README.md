# `base-execution-payload-builder`

Payload builder for Base.

## Overview

Implements Base payload building and validation for the Base execution node. The
`BasePayloadBuilder` assembles new execution payloads from transaction pool contents and
`BasePayloadBuilderAttributes` received from the consensus layer. `BaseExecutionPayloadValidator`
verifies
built payloads against consensus rules. Also provides data availability configuration via
`BaseDAConfig` for fee calculation.

## Stale-storage meter

The log-only stale-storage meter counts, for each committed transaction, the pre-existing
storage slots it rewrites that no canonical block wrote in the last 24 hours, in contracts
with more than 64 changed slots. Many such slots make the serial state root slow. The meter
logs and counts what dry-run limits would have flagged and never excludes a transaction. It is
off by default; enable it with `--payload.stale-storage-metering`.

Write recency lives in `StaleStorageCache`, a keyed 8-way set-associative cache. Each entry
packs a 24-bit fingerprint of the slot's keyed `SipHash-2-4` and an hourly stamp into four bytes,
and each set fits in half a cache line. A slot reads as recent only if its set holds a matching
fingerprint stamped within the window, so every error over-meters: an evicted or never-seen
slot reads as stale. The only false recent reading is a fingerprint collision, about one probe
in two million against a full set.

The cache defaults to 128 mebibytes (about 33.5 million entries). Base Mainnet writes an estimated 10
million distinct slots a day, which overflows about 0.04% of sets within the window. Set the
size with `--payload.stale-storage-cache-mib` (env `PAYLOAD_STALE_STORAGE_CACHE_MIB`), a power
of two from 8 to 4096.

To decide whether the cache is large enough, compare
`base_payload_stale_storage_cache_unexpired_evictions_total` with
`base_payload_stale_storage_cache_writes_total`. An unexpired eviction pushes out a slot written
within the window, which then over-meters until it is written again. Raise the size if
unexpired evictions exceed about 1% of writes. `base_payload_stale_storage_cache_occupancy_ratio`
samples the share of entries that are within the window.

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
