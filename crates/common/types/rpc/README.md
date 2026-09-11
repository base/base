# `base-common-types-rpc`

Shared RPC schemas for Base execution and Ethereum L1 access.

## Overview

This crate owns block, transaction, receipt, log, filter, proof, txpool, debug/trace
simulation schemas. `BaseTransaction`, `BaseTransactionRequest`, and `BaseTransactionReceipt`
retain Base deposit and EIP-8130 behavior; the generic Ethereum types remain available for L1
providers. Shared `Header` and `Log` responses carry optional millisecond timestamps.

The Base network marker and transaction builders live in `base-common-client-ethereum`. RPC-to-EVM
conversion lives in `base-execution-evm-runtime`, keeping these schemas independent of providers and execution.
Engine payload and fork-choice types live in `base-common-types-payload`.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-common-types-rpc = { workspace = true }
```

```rust,ignore
use base_common_types_rpc::{BaseTransactionReceipt, L1BlockInfo};

let receipt: BaseTransactionReceipt = provider.get_transaction_receipt(hash).await?;
let l1_fee = receipt.l1_block_info.l1_fee;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).

Response traits, block transaction representations, inclusion metadata and transaction builder capabilities were consolidated here from the locally maintained Alloy network-primitives crate. Their wire encodings and client contracts are retained.
