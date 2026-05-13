# base-precompile-macros

Procedural macros for type-safe EVM storage abstractions for Base native precompiles.

## Macros

- `#[contract]` — transforms a storage layout struct into a full contract
- `#[derive(Storable)]` — generates storage I/O for structs and `#[repr(u8)]` enums
- `storable_rust_ints!()`, `storable_alloy_ints!()`, `storable_alloy_bytes!()` — primitive impls
- `storable_arrays!()`, `storable_nested_arrays!()` — fixed-size array impls
- `gen_storable_tests!()` — proptest round-trip tests for all storage types
