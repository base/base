# `base-common-network`

Base chain network types and RPC behavior abstraction.

## Overview

Defines the `Base` network type that implements the `alloy_network::Network` trait with Base
transaction and receipt types. This provides a consistent interface to alloy providers and signers
regardless of Base-specific RPC changes.
It also provides the `BaseEngineApi` extension trait for Base-specific Engine API RPC methods.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-common-network = { workspace = true }
```

```rust,ignore
use base_common_network::{Base, BaseEngineApi};
use alloy_provider::ProviderBuilder;

let provider = ProviderBuilder::new().network::<Base>().on_http(url);
let _ = provider.exchange_capabilities(vec![]).await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
