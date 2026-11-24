# Tiger client

Tiger is a Base execution client written in Rust without reth or revm. It validates and imports
Base blocks driven by the Base consensus node over the Engine API, keeps its own persistent state,
and serves the JSON-RPC methods Base nodes use. [SCOPE.md](SCOPE.md) lists what it does and what
it leaves out. It follows [TigerStyle](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md):
simple explicit code, assertions, bounded resources, and few dependencies.

Tiger is its own Cargo workspace, separate from the base workspace, so its dependency tree and
lockfile stay small. Build and test from this directory:

```sh
cargo build --release
cargo test
```

## Layout

| Directory | Crate | What it holds |
| --- | --- | --- |
| `evm/` | `base-tiger-evm` | Base block execution on evm2: fork schedule, transactions, fees, precompiles, system calls, upgrade transitions. |
| `node/` | `base-tiger` | The `tiger` binary: node, Engine API, JSON-RPC, and the differential fuzz adapter. |

## Dependencies

Every direct dependency and why it is here. Anything not listed is pulled in by these.

| Dependency | Reason |
| --- | --- |
| `evm2` | The EVM interpreter, gas schedule, and standard precompiles. |
| `alloy-primitives` | Addresses, hashes, U256, keccak. |
| `alloy-rlp` | RLP encoding of headers, transactions, receipts and trie nodes. |
| `alloy-consensus`, `alloy-eips` | Ethereum header, transaction, receipt and EIP types that Base types build on. |
| `base-common-consensus` | Base transaction and receipt envelopes, deposits, L1 info decoding, predeploy addresses. |
| `base-common-genesis` | Base upgrade list and feature gates, rollup and system config types. |
| `base-common-l1-fees` | L1 data fee and operator fee math. |
| `base-common-flz` | FastLZ size estimate for Fjord L1 fees and the Jovian DA footprint. |
| `base-common-base-tx` | Intrinsic gas schedule shared with the Base transaction type. |
