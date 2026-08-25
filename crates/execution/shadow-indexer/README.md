# `base-shadow-indexer`

Shadow indexer Execution Extension (`ExEx`) that captures reorged-out and reverted execution
blocks and persists their metadata to the shadow indexer database. Canonical blocks are not
persisted: only blocks the chain discarded carry shadow-block signal.

A `ChainReorged` names the canonical replacement only for heights its `new` chain covers. The
shadow builder swaps its speculative chain one Engine API round trip at a time, so `new` is
routinely a single block against five displaced ones, and the rest of the replacements arrive
as later `ChainCommitted` notifications. Those commits are forwarded to the writer purely to
fill in `canonical_hash` on the rows already stored at those heights.

`shadow_blocks` is keyed by `number` alone. Because canonical blocks are not persisted, a
height holds at most one discarded candidate, and a second reorg at that height replaces the
row outright rather than accumulating a sibling. The replacement clears `canonical_hash`: the
hash belonged to the block that was displaced, not to the one now stored.
