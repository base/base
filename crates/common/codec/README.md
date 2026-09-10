# `base-common-codec`

Derive macros for compact persisted encodings and their round-trip tests. The macro crate has
no runtime dependency on its consumers.

`Compact` encodes field-presence flags alongside compact field data. `CompactZstd` uses the
explicit compressor and decompressor paths supplied by a `reth_zstd` attribute. Existing helper
attribute names remain stable so stored encodings and derive behavior do not change.

`add_arbitrary_tests` and `generate_tests` generate compact or RLP round-trip tests. Callers provide
their property-test dependencies. Run the macro unit tests and state-type encoding tests together:

```sh
cargo test -p base-common-codec
cargo test -p base-execution-state-types --features reth-codec,serde-bincode-compat,test-utils
```
