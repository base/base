# base-precompile-storage

EVM storage abstractions and runtime traits for Base native precompiles.

## Execution engines

Native precompile logic is engine-neutral: it reads and writes state through
`PrecompileStorageProvider` using Base-owned types (`AccountInfo`, `Bytecode`,
`StorageCheckpoint`, `PrecompileOutput`). Each execution engine plugs in behind a cargo feature:

| Feature | Provider | Engine |
| --- | --- | --- |
| `revm` | `EvmPrecompileStorageProvider` (metered), `JournalStorageProvider` (gas-free) | revm via alloy-evm `EvmInternals` |
| `evm2` | `Evm2PrecompileStorageProvider` (metered) | evm2 `Evm` host |
| `test-utils` | `HashMapStorageProvider` | in-memory test double |

Workspace dependents take the crate with `default-features = false`, so consumers that need an
engine provider enable its feature explicitly.

### Gas parity

Both metered providers charge identical gas for identical calls; consensus depends on it.

- Account reads (`with_account_info`, `with_account_code`) charge the warm read cost plus the cold
  account surcharge when the account was cold. The access warms the account.
- `sload` charges the warm read cost plus the cold storage surcharge. It also warms the account.
  An out-of-gas read leaves neither the account nor the slot warm.
- `sstore` rejects static calls, then halts out of gas when remaining gas is at or below the
  EIP-2200 call stipend. Otherwise it charges the SSTORE static and dynamic cost and records the
  EIP-3529 refund for the gas schedule passed to the provider. An out-of-gas write is reverted.
- `set_code` rejects static calls, charges the code deposit cost, and charges `CREATE` plus the
  code-hash Keccak cost when the account had no code (regardless of balance).
- `tload`/`tstore` charge the warm read cost; `tload_unmetered` charges nothing.
- `emit_event` charges `LOG` plus the per-topic and per-byte log cost.
- State gas (EIP-8037) is drawn from regular gas; the provider reports no reservoir.

Base native precompiles price storage with the Osaka gas schedule on both engines.

An engine's precompile frame rebuilds its gas tracker from the returned `PrecompileOutput`:
`gas_used` is spent from the call gas limit, `gas_refunded`, `state_gas_used` and
`state_gas_spilled` are copied, and the reservoir becomes `PrecompileOutput::reservoir`. A
`gas_used` above the limit, or an out-of-gas halt, consumes all call gas. A fatal error aborts
execution.

evm2 precompile dispatch does not see the transaction environment, so the EVM's instance
extension must implement `Evm2TxOrigin` to supply `tx.origin`.

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

The namespace can also be declared once on a reusable `Storable` layout type. A `#[contract]`
field with that type is automatically mounted at the type's namespace root:

```rust,ignore
#[derive(Debug, Clone, Storable)]
#[namespace("b20.asset")]
pub struct B20AssetStorage {
    pub shares_to_tokens_ratio: U256,
}

#[contract]
pub struct B20Asset {
    pub security: B20AssetStorage,
}
```

### Mapping slot derivation

```text
slot(key, base) = keccak256(lpad32(key) ‖ to_be32(base))
```

This matches Solidity's `keccak256(abi.encode(key, slot))` for:
- Unsigned integers, `Address`, `FixedBytes<32>` — identical encoding
- `String` — uses `keccak256(bytes(key) ‖ to_be32(base))`, matching Solidity's string-keyed
  mapping derivation
- Signed integers — diverges (we zero-left-pad the two's complement bits; Solidity sign-extends)
- `FixedBytes<N>` for N < 32 — diverges (we left-pad; Solidity right-pads)

Use contract view functions rather than off-chain keccak reconstruction for the divergent types.

### Append-only rule

**Never reorder or reuse storage slots across upgrades.** Adding new fields is safe as long as
they append after existing ones. Changing slot assignments for existing fields corrupts state.

## Attribution

This crate includes code adapted from Tempo's `precompiles` crate, including its storage
abstractions, in the
[`tempoxyz/tempo`](https://github.com/tempoxyz/tempo/tree/main/crates/precompiles)
repository. The upstream license notices are retained in `LICENSE-MIT` and
`LICENSE-APACHE`.
