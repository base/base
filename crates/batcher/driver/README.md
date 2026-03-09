# base-batcher-driver

Minimal batch encoding pipeline for the Base batcher.

Takes `SingleBatch` values, compresses them into a channel using
`ChannelOut<BrotliCompressor>` at Brotli-10, and outputs `Frame`s ready to
be submitted to L1 as calldata or blob data.

This crate is intentionally small. It does not manage channel lifecycles,
transaction submission, or receipts — those concerns belong in higher-level
crates. It provides one type, `ChannelDriver`, which is the inner encoding
kernel shared by the action test harness and (eventually) the full batcher
service.
