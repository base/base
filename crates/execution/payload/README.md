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
the contract's hashed address and the first five nibbles of the hashed slot
(`STALE_STORAGE_TRIE_PREFIX_NIBBLES`), the slot's path in the contract's storage trie. A slot
therefore reads as recent when it, or a sibling under the same depth-five trie node, was written
within the window, which approximates whether the upper part of its trie path is still warm.
The meter still counts slots: the gate, the per-block dedupe and the limits use exact slot
identities, and only the recency lookup is shared within a region.

A region shares trie nodes only down to its depth, and a contract with `n` slots has about
`n / 16^d` slots per depth-`d` region. Depth four would put about 15 slots of a million-slot
contract in each region without their deeper branch nodes being warm, hiding cold faults in the
largest contracts, which are the meter's main target. Depth five keeps regions near one slot up
to a million slots. In a Base Mainnet replay of five complete hours plus a partial sixth, it
left stale counts in contracts with under a thousand written slots unchanged, removed 2.4% of
distinct keys over the whole capture, and changed no dry-run verdict.

Each entry packs a 16-bit fingerprint of the key and a 16-bit hour stamp into four bytes, and
each set fits in half a cache line. Stamps count hours from a base hour fixed when the
cache is built, one window before the wall-clock hour, so every reader decodes every entry
against the same base. Ages are exact and never wrap: an entry left untouched for any length of
time reads as stale. The stamp covers about seven years; past that the recorder stops writing,
logs a warning once, and every slot reads stale until the process restarts. A slot reads as
recent only if its set holds a matching fingerprint stamped within the window, so every error
over-meters: an evicted or never-seen region reads as stale. Beyond the intended sharing within
a region, the only false recent reading is a fingerprint collision, at most about one probe in
8,000 against a set full of live entries. At expected load that is about 6 falsely recent slots
a day across Base Mainnet (about 0.0002 per block), and at most about 21 a day if every set were
full of live entries. That is negligible for a log-only meter; revisit the fingerprint width
before enforcing limits. The per-process secret keeps set placement and fingerprints
unpredictable even though trie paths are public.

The recorder classifies every slot of a canonical block against the cache as it was before the
block, then stamps the block's regions, so trie siblings written in the same block do not make
each other read recent. On a reorg it first empties the entries for every changed slot of the
reverted blocks, so writes that left the canonical chain cannot keep a region recent; a region
the new chain also wrote reads stale until the new chain is stamped. The recorder never clears
the cache. It skips a block, counting its changed slots in
`base_payload_stale_storage_cache_dropped_writes_total`, when the block predates the base hour
(as while the node syncs), is past the stamp range, or is timestamped more than two hours before
the last recorded block.

The cache defaults to 128 mebibytes (about 33.5 million entries). Base Mainnet writes an
estimated 10 million distinct slots a day, and fewer distinct regions. Ten million live keys is
under a third of the cache's entries, so no key is in excess of total capacity, but keys hash to
sets unevenly: at about 2.4 keys per set on average, about 0.08% of sets receive more than eight
within the window. Only the keys beyond eight in those overflowed sets are evicted early, about
0.045% of live keys. Set the size with `--payload.stale-storage-cache-mib`
(env `PAYLOAD_STALE_STORAGE_CACHE_MIB`), a power of two from 8 to 4096.

To decide whether the cache is large enough, compare
`base_payload_stale_storage_cache_unexpired_evictions_total` with
`base_payload_stale_storage_cache_writes_total`. An unexpired eviction pushes out a region written
within the window, whose slots then over-meter until one of them is written again. Raise the size if
unexpired evictions exceed about 1% of writes. Ages are exact, so replacing an entry older than
the window never counts as an unexpired eviction. `base_payload_stale_storage_cache_occupancy_ratio`
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
