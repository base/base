# base-precompile-storage

EVM storage abstractions and runtime traits for Base native precompiles.

## Slot Derivation Rules

### Auto-allocation

Fields in a `#[contract]` struct are allocated sequentially following Solidity's right-to-left
bin-packing rules. Fields smaller than 32 bytes are packed into the same slot when they fit.

```rust,ignore
#[contract]
pub struct MyToken {
    pub name: String,       // slot 0 (full slot — dynamic)
    pub symbol: String,     // slot 1 (full slot — dynamic)
    pub decimals: u8,       // slot 2, offset 0 (1 byte)
    pub paused: bool,       // slot 2, offset 1 (packed with decimals)
    pub total_supply: U256, // slot 3 (doesn't fit with the 30 remaining bytes)
}
```

### Manual slot override

- `#[slot(N)]` — places the field at an explicit absolute slot with offset 0.
- `#[base_slot(N)]` — resets the auto-allocation chain starting from slot N.
- `#[slot("key")]` — computes `keccak256("key")` at macro expansion time.

### Namespaced layouts

- `#[namespace("id")]` — starts a `#[contract]` field at the ERC-7201 root for `id`.

Multiple fields with the same namespace use normal Solidity offsets from that root without advancing
the surrounding contract layout. `#[slot]` and `#[base_slot]` overrides cannot be combined with
`#[namespace]` on the same field.

### Mapping slot derivation

```text
slot(key, base) = keccak256(lpad32(key) ‖ to_be32(base))
```

This matches Solidity's `keccak256(abi.encode(key, slot))` for:
- Unsigned integers, `Address`, `FixedBytes<32>` — identical encoding
- Signed integers — diverges (we zero-left-pad the two's complement bits; Solidity sign-extends)
- `FixedBytes<N>` for N < 32 — diverges (we left-pad; Solidity right-pads)

Use contract view functions rather than off-chain keccak reconstruction for the divergent types.

### Append-only rule

**Never reorder or reuse storage slots across hardforks.** Adding new fields is safe as long as
they append after existing ones. Changing slot assignments for existing fields corrupts state.

## Attribution

This crate includes code adapted from Tempo's `precompiles` crate, including its storage
abstractions, in the
[`tempoxyz/tempo`](https://github.com/tempoxyz/tempo/tree/main/crates/precompiles)
repository. The upstream license notices are retained in `LICENSE-MIT` and
`LICENSE-APACHE`.
