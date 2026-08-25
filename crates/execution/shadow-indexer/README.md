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

Rows and canonical refs travel as one ordered `ShadowWrite` stream and reach the database in
that order. A ref resolves whichever candidate is stored at its height when it is applied, so
applying it out of order would pin one block's replacement hash onto a different block.
Consecutive writes of the same kind still collapse into one statement, which keeps a backfill
notification to a single round trip.

A flush that exhausts its retries drops its rows but keeps its canonical refs. A dropped row
costs that block's metrics; a dropped ref would strand a row persisted by an earlier flush at
`NULL` forever, since the reader skips unresolved rows and nothing else revisits them.
