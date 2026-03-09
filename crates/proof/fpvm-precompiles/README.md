# `base-proof-fpvm-precompiles`

FPVM-accelerated precompiles for the Base proof client.

## Overview

Provides optimized EVM precompile implementations for Fault Proof VM execution. Exports
`OpFpvmPrecompiles` (the FPVM-specific precompile set) and `FpvmOpEvmFactory` (an EVM factory
wired with those precompiles), enabling the proof client to handle cryptographic precompile
calls efficiently within the constrained FPVM environment.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-proof-fpvm-precompiles = { workspace = true }
```

```rust,ignore
use base_proof_fpvm_precompiles::FpvmOpEvmFactory;

let factory = FpvmOpEvmFactory::new();
let evm = factory.create_evm(db, env);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
