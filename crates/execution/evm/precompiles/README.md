# `base-execution-evm-precompiles`

Base precompile selection, native contracts, storage access, and EIP-8130 account authorization.

`BasePrecompiles` selects the Ethereum and Base precompile set for an execution upgrade and
installs native contracts into `PrecompilesMap`. The crate owns the dispatch contracts used by
the EVM runtime, the activation registry, B-20 tokens and policies, transaction context, and nonce
management. Crypto implementations remain in the separate crypto backend crate.

With `std`, the crate also provides EIP-8130 signature verification, actor authorization, account
configuration changes, nonce validation, intrinsic gas, and fee checks. These use the same storage
interfaces as the native contracts. Runtime, transaction pool, and payload builder callers share
these implementations.

Historical execution upgrade behavior is preserved. The `std` feature controls the authorization
helpers and standard-library support; `test-utils` enables fixtures and the native contract test
suites. Crypto backend features are forwarded to the crypto implementation.

Run unit tests and all native contract integration suites with:

```sh
cargo test -p base-execution-evm-precompiles --features test-utils
```

Check the allocation-only configuration on a bare-metal target with:

```sh
cargo check -p base-execution-evm-precompiles --no-default-features --target riscv32imac-unknown-none-elf
```
