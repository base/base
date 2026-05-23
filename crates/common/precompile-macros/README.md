# base-precompile-macros

Procedural macros for type-safe EVM storage abstractions for Base native precompiles.

## Macros

- `#[contract]` — transforms a storage layout struct into a full contract
- `#[namespace("id")]` — starts a `#[contract]` field/layout or a `Storable` layout type at
  an ERC-7201 namespace root
- `#[derive(Storable)]` — generates storage I/O for structs and `#[repr(u8)]` enums
- `storable_rust_ints!()`, `storable_alloy_ints!()`, `storable_alloy_bytes!()` — primitive impls
- `storable_arrays!()`, `storable_nested_arrays!()` — fixed-size array impls
- `gen_storable_tests!()` — proptest round-trip tests for all storage types

For `Storable` layouts, place the derive before the namespace helper:

```rust,ignore
#[derive(Debug, Clone, Storable)]
#[namespace("b20")]
pub struct B20Storage {
    pub total_supply: U256,
}

#[contract]
pub struct B20Security {
    pub b20: B20Storage,
}
```

## Attribution

This crate includes code adapted from Tempo's `precompiles-macros` crate in the
[`tempoxyz/tempo`](https://github.com/tempoxyz/tempo/tree/main/crates/precompiles-macros)
repository. The upstream license notices are retained in `LICENSE-MIT` and
`LICENSE-APACHE`.
