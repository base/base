# `base-common-codec-storage`

Dictionary-backed Zstandard codecs for persisted transactions and receipts. `StorageCodec`
reuses thread-local compressors with `std`; allocation-only callers create codec instances as needed.
`ReusableDecompressor` retains its output buffer across operations.

The committed transaction and receipt dictionaries are part of the storage format. Package
organization must preserve them and the compression settings.

```sh
cargo test -p base-common-codec-storage
cargo test -p base-common-codec-storage --no-default-features
```

The Zstandard backend is a native library even when Rust-side thread-local caching is disabled.
