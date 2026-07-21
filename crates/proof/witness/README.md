# `base-proof-witness`

Builds the preimage vector consumed by Base fault-proof backends.

The generator fetches hash-pinned L1 derivation data and L2 execution witnesses concurrently,
validates and deduplicates their content-addressed preimages, and returns the existing
`Vec<(PreimageKey, Vec<u8>)>` wire format.
