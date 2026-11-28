# base-tiger-precompile-storage

Storage helpers and the provider boundary for the Base native precompiles in the tiger
client, ported from base/base's `base-precompile-storage` without revm. `PrecompileStorageProvider`
is the boundary an execution engine implements (storage, transient storage, logs, account and code
access, gas, checkpoints); `GasSchedule` is the fixed gas table base-reth charges for that work.

`StorageCtx` is the scoped handle a precompile call works through. `StorageLayout` and the
`StorageCtx` read/write helpers in `layout.rs` implement the Solidity storage layouts the
precompiles use, by hand:

- ERC-7201 namespaces: a layout's fields sit at `root + offset`, where
  `root = keccak256(keccak256(id) - 1) & ~0xff`. Each precompile storage module hard-codes its
  root and field offsets and tests the root against `StorageLayout::erc7201_root`.
- Values are right-aligned in their word. Fields narrower than 32 bytes that share a word are
  read with one SLOAD and written with an SLOAD and an SSTORE of the merged word.
- Mappings: `slot(key) = keccak256(lpad32(key) ‖ to_be32(base))` for unsigned integers,
  addresses and `bytes32`, and `keccak256(bytes(key) ‖ to_be32(base))` for strings. A nested
  mapping applies the derivation again from the outer entry's slot.
- Strings use Solidity's short (inline, `len * 2` in the last byte) and long (`len * 2 + 1` in
  the head, data at `keccak256(slot)`) encodings; `uint64[]` stores its length at the slot and
  four elements per word at `keccak256(slot)`, the first in the low bytes. From Cobalt
  (`StorageFeatures::dynamic_storage_tail_cleanup_enabled`), writes of both first read the old
  value's length and zero the data words it no longer needs.

These precompiles run in consensus, so the order and count of storage accesses each helper makes
is part of its contract (it decides gas) and is documented on it.

### Append-only rule

**Never reorder or reuse storage slots across upgrades.** Adding new fields is safe as long as
they append after existing ones. Changing slot assignments for existing fields corrupts state.

## Attribution

This crate includes code adapted from Tempo's `precompiles` crate, including its storage
abstractions, in the
[`tempoxyz/tempo`](https://github.com/tempoxyz/tempo/tree/main/crates/precompiles)
repository. The upstream license notices are retained in `LICENSE-MIT` and
`LICENSE-APACHE`.
