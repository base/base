# `base-exoware-archive`

Exoware archive store: exports finalized Base chain history (headers, bodies, receipts,
senders, account and storage before-value changesets) into an Exoware key-value store and
serves verified reads from it, with no S3 bucket, `NippyJar` files or `SlateDB`.

## Overview

- **Format**: fixed-width keys `gen ‖ family ‖ fields ‖ (segment | block):u64be`, canonical
  RLP/EIP-2718 values, offset-set postings, a config row and a per-segment marker carrying a
  SHA-256 digest of the segment's rows (see [Marker digest](#marker-digest)). Block-family
  values too large for one store value continue in part rows (see
  [Continuation parts](#continuation-parts)).
- **Publication** (`SegmentPublisher`): rows are a pure function of finalized block data, so
  any number of nodes can publish the same segment without compare-and-swap. A segment
  becomes visible when its marker is written, after every row is acknowledged and after the
  previous segment's marker exists. Segments are streamed in batches (see
  [Exporter memory](#exporter-memory)).
- **Reads** (`ArchiveReader`): every request resolves the highest marker in one monotonic
  session; block, hash and receipt answers are verified against roots and header hashes;
  state reads use the first change after the block and its before-value; logs use posting
  algebra and an exact filter on verified receipts.
- **Audit** (`SegmentAuditor`): rebuilds a segment from its stored block rows and compares
  every row and the digest with the marker.
- **Pruning**: one Exoware `Prune` policy per family, keyed on the trailing number (the block
  number for continuation parts).

## Writer trust model

The config row is created only by `ExowareArchive::provision`, an explicit operator step.
Exporters require it to exist and to match their own config byte for byte. Before writing
any row of segment `k`, and again right before the marker put, the exporter reads marker
`k`. If the marker is equal, the segment is skipped. If it differs, export halts and the
`quarantined` gauge is set. These checks are diagnostics, not fencing: Exoware has no
conditional writes, so concurrent writers with incompatible data are unsupported.

## Guarantees

Answers for block, header, body, receipts, senders and hash locators are verified on every
request, against the header hash, transaction root, receipt root, sender recovery and either
the node's canonical hash or the segment's hash chain up to the marker. State and log
answers detect false positives only. Their completeness relies on a store that is trusted
and not corrupt, plus on-demand `SegmentAuditor::audit_segment`, which recomputes the digest
over every row of a segment.

Regolith-window deposit nonces are in the same class. A deposit receipt's nonce is not covered
by the receipt root, so the reader can only check it for consistency: against the sender's
before-value nonce in the block's account changes plus the earlier transactions from the same
sender in that block. Neither value is authenticated by any root, so a store that rewrites both
consistently is not detected on read. Like state and log completeness (A3), these nonces are
trusted-store fields; `SegmentAuditor` detects such a rewrite against the marker digest.

Sender recovery on read is a deliberate cost: every block read recovers each transaction's
signer (one secp256k1 recovery per non-deposit transaction, on the order of tens of
microseconds each) and compares it with the archived senders, so a full block of a few hundred
transactions adds milliseconds of CPU per request. Hash and root checks alone would not catch
a corrupt sender row.

## Pruning

Grace defaults to 2 segments and must be at least 1. Every request re-reads the marker after
its last read, in the same session, and fails with `Pruned` if anything it touched fell
below the new earliest block, so it never returns a partial answer. `run_until` repeats the
prune periodically, because a stalled exporter can reinsert old rows. Key GC does not reclaim
the Exoware sequence log; `SetRetention` keep-latest is an operator setting.

## Encoding

Typed transactions are raw EIP-2718 bytes placed directly in the body's RLP list, with no
extra RLP string wrapping. An absent account before-value is `present = 0` with every
other field zeroed. Receipts are stored in full, including the deposit nonce, even when
the Regolith receipt root excludes it.

## Continuation parts

A store value holds at most `MAX_VALUE_LEN` (32 `MiB`) bytes, and a Base block can carry more
body or receipt data than that. Every block family (`0x10`–`0x15`) therefore splits values at
`VALUE_PART_LEN`, a format constant equal to `MAX_VALUE_LEN`:

- A value shorter than `VALUE_PART_LEN` is one row at `gen ‖ family ‖ n`, unchanged.
- A longer value, or one of exactly `VALUE_PART_LEN` bytes, puts its first `VALUE_PART_LEN`
  bytes at `gen ‖ family ‖ n` and the following slices at continuation keys
  `gen ‖ family ‖ n ‖ part:u16be`, part 1 first. Every part but the last is exactly
  `VALUE_PART_LEN` bytes; when the length is a multiple of it, a final empty part follows, so a
  part shorter than `VALUE_PART_LEN` always ends the value. A value needing more than
  `u16::MAX` continuations is refused with `TooManyParts`.

Continuation keys sort after their base key and before block `n + 1`'s key, so block-family
range scans and the audit see them in order. They are ordinary segment rows: the marker's
`row_count` and digest cover them, the audit reports a missing part as a missing row, and
pruning deletes them with their block. A reader reads part 1, 2, … only after a base value of
exactly `VALUE_PART_LEN` bytes, so a value below the limit costs no extra read; a missing or
over-long part is `Unavailable`, never a truncated value.

## Marker digest

The marker's digest is `SHA-256(D(0x01) ‖ D(0x02) ‖ D(0x03) ‖ D(0x04) ‖ D(0x05) ‖ D(0x10) ‖ …
‖ D(0x15))`: the concatenation, in family-byte order, of one SHA-256 per family. `D(f)` hashes
that family's rows of the segment in ascending key order, each framed as
`len(key):u64be ‖ key ‖ len(value):u64be ‖ value`; a family with no rows contributes the
SHA-256 of empty input. Config (`0x00`) and marker (`0x1f`) rows are not segment rows. Because
families are hashed independently, the exporter hashes block rows as it streams them and the
audit recomputes the digest the same way, a batch at a time.

## Exporter memory

The exporter never holds a whole segment of blocks. It loads `export_batch_blocks` (CLI
`--archive.exoware.export-batch-blocks`, default 256) consecutive blocks, verifies them,
encodes their block-family rows (`0x10`–`0x15`), folds them into the digest, writes them chunk
by chunk and drops the batch before loading the next. Only the index rows and postings
(`0x01`–`0x05`: block and transaction locators, state and log postings) and the eleven running
family digests accumulate. They are written after the last batch, then the marker last, so a
partial segment stays invisible and a replay writes identical bytes; recovery, contiguity and
quarantine checks are unchanged. Peak block memory is one batch: with the default 256 blocks
of an 8192-block segment, 1/32 of the previous whole-segment load. Index memory grows with the
segment's distinct transactions, touched accounts and slots, and log addresses and topics, not
with block bodies. The audit reads stored block rows in batches of the same size.

## Log reads

A log query's candidate blocks (at most `max_log_candidates`, default 10,000) are read in
chunks of `LOG_BLOCKS_PER_READ` (32) blocks: one `get_many` of header, body and receipts per
chunk, decoded, verified and filtered, then dropped. With a `max_logs` cap the reader stops
after the chunk whose matches crossed it and returns `TooManyLogs` with the crossing block.
