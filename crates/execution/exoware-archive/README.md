# `base-exoware-archive`

Exoware archive store: exports finalized Base chain history (headers, bodies, receipts,
senders, account and storage before-value changesets) into an Exoware key-value store and
serves verified reads from it, with no S3 bucket, NippyJar files or SlateDB.

## Overview

- **Format**: fixed-width keys `gen ‖ family ‖ fields ‖ (segment | block):u64be`, canonical
  RLP/EIP-2718 values, offset-set postings, a config row and a per-segment marker carrying a
  SHA-256 digest of the segment's rows.
- **Publication** (`SegmentPublisher`): rows are a pure function of finalized block data, so
  any number of nodes can publish the same segment without compare-and-swap. A segment
  becomes visible when its marker is written, after every row is acknowledged and after the
  previous segment's marker exists.
- **Reads** (`ArchiveReader`): every request resolves the highest marker in one monotonic
  session; block, hash and receipt answers are verified against roots and header hashes;
  state reads use the first change after the block and its before-value; logs use posting
  algebra and an exact filter on verified receipts.
- **Audit** (`SegmentAuditor`): rebuilds a segment from its stored block rows and compares
  every row and the digest with the marker.
- **Pruning**: one Exoware `Prune` policy per family, keyed on the trailing number.

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
