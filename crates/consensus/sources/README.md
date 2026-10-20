# `base-consensus-sources`

<a href="https://crates.io/crates/base-consensus-sources"><img src="https://img.shields.io/crates/v/base-consensus-sources.svg" alt="base-consensus-sources crate"></a>
<a href="https://specs.base.org"><img src="https://img.shields.io/badge/Docs-854a15?style=flat&labelColor=1C2C2E&color=BEC5C9&logo=mdBook&logoColor=BEC5C9" alt="Docs" /></a>

Data source types and utilities for the base-consensus-node.

## Overview

Defines block signing interfaces for the consensus node. Provides `BlockSigner` and
`BlockSignerHandler` with `Local` and `Remote` variants for signing with a local private key or
a remote service. `RemoteSigner` and `RemoteSignerHandler` delegate signing to the external service,
and certificate handling types (`ClientCert`, `CertificateError`) support mTLS authentication
with remote signers.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-consensus-sources = { workspace = true }
```

```rust,ignore
use alloy_primitives::B256;
use alloy_signer::{Signature, Signer};
use alloy_signer_local::PrivateKeySigner;
use base_common_rpc_types_engine::PayloadHash;
use base_consensus_sources::BlockSigner;

async fn sign_payload(
    key: B256,
    payload_hash: PayloadHash,
    chain_id: u64,
) -> Result<Signature, Box<dyn std::error::Error>> {
    let local_signer = PrivateKeySigner::from_bytes(&key)?.with_chain_id(Some(chain_id));
    let address = local_signer.address();
    let signer: BlockSigner = local_signer.into();
    let handler = signer.start().await?;
    Ok(handler.sign_block(payload_hash, chain_id, address).await?)
}
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
