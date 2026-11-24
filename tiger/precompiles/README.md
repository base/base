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

`lookup(address)` resolves a call target: fixed addresses first, then B-20 token addresses, the
same order base-reth's `PrecompilesMap` uses. `fixed()` lists the fixed addresses, which are the
ones the EVM pre-warms; token addresses are resolved dynamically and are not pre-warmed.
`run(precompile, address, calldata, is_direct_call, provider)` executes one call against any
`PrecompileStorageProvider` (from `base-tiger-precompile-storage`); `DELEGATECALL`/`CALLCODE`
calls revert with `DelegateCallNotAllowed()` and no gas used.

The storage provider for evm2 lives in the tiger evm crate. Tests use the in-memory
`HashMapStorageProvider` behind the `test-utils` feature.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
