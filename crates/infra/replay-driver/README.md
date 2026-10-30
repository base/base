# base-replay-driver

Derives shadow-build payload attributes from a trusted canonical block using the
chain's fork schedule. Only the leading deposits are forced; the builder remains
free to select transactions from its pool. Holocene and Jovian fee parameters
come from the canonical block's extra data, not its parent's.

This is the attribute-derivation slice of CHAIN-6449. The Engine API shadow loop
and candidate records require a follow-up issue: build on the canonical parent,
record and discard the candidate, then import and select the canonical block.
