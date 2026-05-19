# `base-common-precompiles`

Base precompile definitions and fork-specific precompile sets.

## Overview

Provides Base-specific precompile selection on top of revm's Ethereum precompile provider. The
crate owns the Base precompile schedule, including fork-specific additions, removals, and input
limit overrides for upgrades such as Fjord, Granite, Isthmus, Jovian, Azul, and later upgrades.

The public API is intentionally small. `BasePrecompiles` builds the correct precompile provider for
a Base upgrade, while `BasePrecompileSpec` is the lightweight trait bound used by downstream crates
that wrap `BaseUpgrade` in their own spec type. Most EVM consumers should continue to use the
`BasePrecompiles` alias exposed by `base-common-evm`, because that alias is already wired to
`BaseSpecId`.

## Behavior

Base upgrades before Fjord use the matching Ethereum precompile set for their execution spec. Fjord
adds RIP-7212 secp256r1 verification, Granite overrides the bn254 pairing precompile input limits,
Isthmus adds the Prague BLS12-381 precompiles with Base-specific limits, and Jovian tightens the
variable-input bn254 and BLS12-381 limits. Azul, Beryl, and newer Base upgrades inherit the latest
known Base precompile set until they are explicitly mapped.

Starting in Beryl, `BasePrecompileInstaller` also installs the activation registry precompile at
`0x84530000000000000000000000000000000000ff`. The registry stores runtime feature flags keyed by
`bytes32`, defaults every feature to inactive, and exposes `isActivated(bytes32)`, `admin()`,
`activate(bytes32)`, and `deactivate(bytes32)`. Only the configured activation admin can mutate
feature state, and repeated no-op transitions revert.

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
