# `base-tiger-precompiles`

The Base native precompiles for the tiger client: the activation registry, the policy registry,
the B-20 token factory, B-20 asset and stablecoin tokens, the transaction context, and the nonce
manager. This is a port of base/base's `base-common-precompiles` with the revm and alloy-evm
surface, the metrics observers, and the Ethereum precompile table removed (the tiger evm crate
selects the Ethereum set and the Base input caps itself). The enshrined logic is unchanged, so a
call charges the same gas, writes the same storage, and emits the same logs as in base-reth.

## Entry point

`NativePrecompiles::new(upgrade, activation_admin)` is the native set a fork installs:

- Beryl: activation registry (`0x8453…0001`), policy registry (`0x8453…0002`), B-20 factory
  (`0xB20F…0000`), and B-20 tokens at `0xb2`-prefixed addresses (asset and stablecoin).
- Cobalt: transaction context (`0x8130…aa02`) and nonce manager; asset, stablecoin and policy
  logic v2; dynamic storage tail cleanup and compact ABI decode errors (`StorageFeatures::Cobalt`);
  the activation registry rejects value and stores its own admin.
- Denim: asset, stablecoin and policy logic v3.

Each precompile has one logic implementation for all its versions, with a check on the version
wherever one differs: `PolicyRegistry` for the policy registry, and `B20Logic` for the B-20 tokens
(the asset and stablecoin kinds share the core; the asset's own methods are in `b20_asset`).
A version dials its precompile's canonical `sol!` interface minus the selectors later forks added
(`B20Version::dials_b20`/`dials_asset`, `PolicyVersion::valid_selector`). Where a Beryl type
differs (`IB20V1`'s `PausableFeature`, `IPolicyRegistryV1`'s `PolicyType`), Beryl calls decode
against the Beryl interface first, so they fail with Beryl's error bytes.

`lookup(address)` resolves a call target: fixed addresses first, then B-20 token addresses, the
same order base-reth's `PrecompilesMap` uses. `fixed()` lists the fixed addresses, which are the
ones the EVM pre-warms; token addresses are resolved dynamically and are not pre-warmed.
`run(precompile, address, calldata, is_direct_call, provider)` executes one call against any
`PrecompileStorageProvider`; `DELEGATECALL`/`CALLCODE` calls revert with
`DelegateCallNotAllowed()` and no gas used.

## Storage

The `storage` module is the boundary to the execution engine. `PrecompileStorageProvider` is what
an engine implements (storage, transient storage, logs, account and code access, gas,
checkpoints); `GasSchedule` is the fixed gas table base-reth charges for that work. `StorageCtx`
is the scoped handle a precompile call works through. `StorageLayout` and the `StorageCtx`
read/write helpers in `storage/layout.rs` implement the Solidity storage layouts the precompiles
use, by hand:

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
is part of its contract (it decides gas) and is documented on it. **Never reorder or reuse
storage slots across upgrades**: new fields append after existing ones.

Each precompile's storage module (`*/storage.rs`, `common/core_storage.rs`) writes its Solidity
layout out by hand: the ERC-7201 namespace root and field slots as constants, and each accessor
as the exact SLOADs and SSTOREs base-reth's macro-generated code made, through the helpers on
`StorageCtx` and `StorageLayout`. The asset and stablecoin tokens are `B20TokenStorage` over their
extension layouts and share one `TokenAccounting` implementation. The `b20_*_golden` tests pin storage access counts and the resulting storage of
every operation.

The storage provider for evm2 lives in the tiger evm crate. Tests use the in-memory
`HashMapStorageProvider`: unit tests through `cfg(test)`, the golden tests through the
`test-utils` feature.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE). The storage
module includes code adapted from the storage abstractions of Tempo's
[`precompiles`](https://github.com/tempoxyz/tempo/tree/main/crates/precompiles) crate.
