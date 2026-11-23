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

Write recency lives in `StaleStorageCache`, a keyed 8-way set-associative cache. An entry
stands for a storage-trie region rather than a single slot: its key is a keyed `SipHash-2-4` of
the contract's hashed address and the first four nibbles of the hashed slot
(`STALE_STORAGE_TRIE_PREFIX_NIBBLES`), the slot's path in the contract's storage trie. A slot
therefore reads as recent when it, or a sibling under the same depth-four trie node, was written
within the window, which approximates whether the upper part of its trie path is still warm.
The meter still counts slots: the gate, the per-block dedupe and the limits use exact slot
identities, and only the recency lookup is shared within a region. Four nibbles is the shallowest
depth that consolidates large contracts without changing the meter for small ones; in a
five-hour Base Mainnet replay it cut distinct cache keys by about a fifth (by about three
quarters in the largest contracts), moved stale counts in contracts with under a thousand
written slots by 0.3%, and changed no dry-run verdict.

Each entry packs a 24-bit fingerprint of the key and an hourly stamp into four bytes, and each
set fits in half a cache line. A slot reads as recent only if its set holds a matching
fingerprint stamped within the window, so every error over-meters: an evicted or never-seen
region reads as stale. Beyond the intended sharing within a region, the only false recent reading
is a fingerprint collision, about one probe in two million against a full set. The per-process
secret keeps set placement and fingerprints unpredictable even though trie paths are public.

The cache defaults to 128 mebibytes (about 33.5 million entries). Base Mainnet writes an
estimated 10 million distinct slots a day, and fewer distinct regions; at 10 million keys about
0.04% of sets overflow within the window. Set the size with `--payload.stale-storage-cache-mib`
(env `PAYLOAD_STALE_STORAGE_CACHE_MIB`), a power of two from 8 to 4096.

To decide whether the cache is large enough, compare
`base_payload_stale_storage_cache_unexpired_evictions_total` with
`base_payload_stale_storage_cache_writes_total`. An unexpired eviction pushes out a region written
within the window, whose slots then over-meter until one of them is written again. Raise the size if
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
