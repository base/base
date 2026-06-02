# `base-common-precompiles`

Base precompile definitions and fork-specific precompile sets.

## Overview

Provides the Base-specific precompile layer used by the Base EVM. The crate selects the static
Ethereum and Base precompile table for each supported `BaseUpgrade`, and it also owns the Beryl
native precompiles that are installed into a dynamic `PrecompilesMap`.

`BasePrecompiles` is the main entry point. It can expose the static precompile table for a fork, or
build the full installed map used by EVM execution. `BasePrecompileSpec` is the small trait bound
that lets downstream crates use their own spec wrapper as long as it converts to and from
`BaseUpgrade`. Most EVM users should still go through the `BasePrecompiles` alias and builders in
`base-common-evm`, because those are already wired to `BaseSpecId` and install the full map.

The crate also exports the ABI types, storage adapters, entry points, and shared token traits for
the native B-20 token system. Those exports cover the activation registry, policy registry, B-20
factory, default B-20 token, stablecoin token, asset token, and the reusable role, pause, policy,
permit, and accounting helpers used by those precompiles.

## Behavior

Bedrock, Regolith, Canyon, and Ecotone use the matching Ethereum precompile set for their execution
spec. Fjord adds RIP-7212 secp256r1 verification. Granite and Holocene use the Fjord set with the
Base bn254 pairing input limit. Isthmus adds the Prague BLS12-381 precompiles with Base-specific
limits. Jovian replaces the variable-input bn254 and BLS12-381 entries with tighter Base limits.
Azul adopts Osaka pricing and bounds for MODEXP and P256VERIFY. Beryl uses the same static table as
Azul.

Only the upgrades explicitly mapped by `BasePrecompiles::new_with_spec` are supported. Passing an
unmapped `BaseUpgrade` panics instead of silently inheriting the previous table.

`precompiles()` returns only the static precompile table. `install()` and `install_with_observer()`
build the execution map, and for Beryl they add the native dynamic precompiles. The dynamic layer
installs the B-20 factory, the dynamic B-20 token lookup, the policy registry, and the activation
registry.

The activation registry lives at `0x8453000000000000000000000000000000000001`. It stores feature
flags keyed by `bytes32`, defaults every feature to inactive, and exposes `isActivated(bytes32)`,
`admin()`, `activate(bytes32)`, and `deactivate(bytes32)`. The optional activation admin configured
with `with_activation_admin_address()` is the only account that can change feature state. Without a
configured admin, `admin()` reports the zero address and activation mutations revert as
unauthorized. B-20 tokens, the B-20 factory, and the policy registry all check the registry before
serving their own ABI calls.

The policy registry is the global B-20 policy precompile at
`0x8453000000000000000000000000000000000002`. It creates and manages allowlist and blocklist
policies, supports staged admin transfer and admin renunciation, and provides built-in policies for
"always allow" and "always block" behavior.

The B-20 factory is the singleton precompile at `0xB20F000000000000000000000000000000000000`. It
creates deterministic B-20 token addresses from the caller, variant, and salt. B-20 token addresses
use the `0xb2` prefix with the variant encoded in the address. The default B-20 variant uses 18
decimals, while stablecoin and asset variants use 6 decimals.

Default B-20 tokens implement the ERC-20 surface plus roles, pausing, supply caps, memo transfers,
policy hooks, EIP-2612 permits, ERC-5267 domain reporting, and ERC-7572 contract metadata. The
stablecoin variant adds currency metadata. The asset variant adds multiplier-based scaling,
minimum redeemable checks, batched mint and burn operations, asset metadata, and announcement
flows that can execute internal token calls.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-common-precompiles = { workspace = true }
```

```rust,ignore
use base_common_chains::BaseUpgrade;
use base_common_precompiles::BasePrecompiles;

let precompiles = BasePrecompiles::new_with_spec(BaseUpgrade::Jovian);
let _active = precompiles.precompiles();
```

Use `install()` when you need the complete execution map, including Beryl native precompiles:

```rust,ignore
use alloy_primitives::address;
use base_common_chains::BaseUpgrade;
use base_common_precompiles::BasePrecompiles;

let activation_admin = address!("0000000000000000000000000000000000000001");
let precompiles = BasePrecompiles::new_with_spec(BaseUpgrade::Beryl)
    .with_activation_admin_address(Some(activation_admin))
    .install();
```

Downstream EVM crates that use a wrapper spec can pass that wrapper directly as long as it converts
to and from `BaseUpgrade`:

```rust,ignore
use base_common_chains::BaseUpgrade;
use base_common_evm::BaseSpecId;
use base_common_precompiles::BasePrecompiles;

let precompiles = BasePrecompiles::new_with_spec(BaseSpecId::new(BaseUpgrade::Azul));
let _active = precompiles.precompiles();
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
