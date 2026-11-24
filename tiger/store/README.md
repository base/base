# `base-tiger-store`

The tiger client's storage layer, which reads and writes base-reth's on-disk datadir format so
tiger can start from a base-reth datadir or snapshot and base-reth can open what tiger writes:
MDBX tables (libmdbx, vendored C sources), static files, and the RocksDB history indices of
reth's storage v2 layout. It also holds the fault-injection layer used to prove crash safety
(`TIGER_FAULT`, see `src/fault.rs`).

`libmdbx/` holds libmdbx 0.13.12 (`mdbx.c`, `mdbx.h`, licenses) as vendored by reth's
`reth-mdbx-sys`, with one tiger patch: hooks before every `pwrite` and after every `fsync`
(search `tiger` in `mdbx.c`). `src/mdbx_sys.rs` holds bindings pre-generated with bindgen 0.72.1
using reth's `reth-mdbx-sys` build script settings.

Canonical block data (`src/blocks.rs`) is written as base-reth writes it: `WriteTxn::append_block`
puts the header, transactions, senders and receipts in the static files and `HeaderNumbers`,
`BlockBodyIndices`, `TransactionBlocks` and `TransactionHashNumbers` in the databases, and
`Datadir::header`, `transactions`, `receipts`, `senders`, `block_number` and
`transaction_location` read them back. The first block (genesis) may have any number; the static
files then start at it as base-reth's `init_genesis` makes them.
