# base-precompile-storage

EVM storage abstractions and runtime traits for Base native precompiles.

## Slot Derivation Rules

### Auto-allocation

Fields in a `#[precompile_storage]` struct are allocated sequentially following Solidity's
right-to-left bin-packing rules. Fields smaller than 32 bytes are packed into the same slot when
they fit.

```rust,ignore
#[precompile_storage(base_slot = "base.native_erc20.token.v1")]
pub struct MyToken {
    name: String,       // slot keccak256("base.native_erc20.token.v1") (full slot, dynamic)
    symbol: String,     // base slot + 1 (full slot, dynamic)
    decimals: u8,       // base slot + 2, offset 0 (1 byte)
    paused: bool,       // base slot + 2, offset 1 (packed with decimals)
    total_supply: U256, // base slot + 3 (doesn't fit with the 30 remaining bytes)
}
```

### Manual slot override

- `#[precompile_storage(base_slot = N)]` — starts auto-allocation at slot N.
- `#[precompile_storage(base_slot = "key")]` — starts auto-allocation at `keccak256("key")`.
- `#[slot(N)]` — places the field at an explicit absolute slot with offset 0.
- `#[base_slot(N)]` — resets the auto-allocation chain starting from slot N.
- `#[slot("key")]` — computes `keccak256("key")` at macro expansion time.

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
