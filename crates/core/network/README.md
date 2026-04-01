# `base-alloy-network`

Base chain network types and RPC behavior abstraction.

## Overview

Defines the `Base` network type that implements the `alloy_network::Network` trait with Base
transaction and receipt types. This provides a consistent interface to alloy providers and signers
regardless of Base-specific RPC changes. Also re-exports alloy response types (`BlockResponse`,
`ReceiptResponse`, `TransactionResponse`) and OP transaction types (`OpTxType`, `OpTxEnvelope`,
`OpTypedTransaction`).

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-alloy-network = { workspace = true }
```

```rust,ignore
use base_alloy_network::Base;
use alloy_provider::ProviderBuilder;

let provider = ProviderBuilder::new().network::<Base>().on_http(url);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
