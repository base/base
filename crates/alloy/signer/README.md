# `base-alloy-signer`

Remote transaction signer that delegates signing to an external signer sidecar via
`eth_signTransaction` JSON-RPC.

## Overview

Provides `RemoteSigner`, a type that implements alloy's `TxSigner<Signature>` trait by forwarding
signing requests to an external signer service over HTTP. This allows
`EthereumWallet::from(remote_signer)` to work seamlessly with the standard alloy signing pipeline.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-alloy-signer = { workspace = true }
```

```rust,ignore
use base_alloy_signer::RemoteSigner;
use alloy_network::EthereumWallet;
use alloy_primitives::Address;
use url::Url;

let signer = RemoteSigner::new(
    Url::parse("http://localhost:8080").unwrap(),
    "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045".parse::<Address>().unwrap(),
).unwrap();
let wallet = EthereumWallet::from(signer);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
