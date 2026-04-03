# `base-consensus-sources`

<a href="https://crates.io/crates/base-consensus-sources"><img src="https://img.shields.io/crates/v/base-consensus-sources.svg" alt="base-consensus-sources crate"></a>
<a href="https://specs.base.org"><img src="https://img.shields.io/badge/Docs-854a15?style=flat&labelColor=1C2C2E&color=BEC5C9&logo=mdBook&logoColor=BEC5C9" alt="Docs" /></a>

Data source types and utilities for the base-consensus-node.

## Overview

Defines block signing interfaces for the consensus node. Provides `BlockSigner` and
`BlockSignerHandler` for local signing, `RemoteSigner` and `RemoteSignerHandler` for delegating
signing to an external service, and certificate handling types (`ClientCert`, `CertificateError`)
for mTLS authentication with remote signers.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-consensus-sources = { workspace = true }
```

```rust,ignore
use base_consensus_sources::{BlockSigner, RemoteSigner};

let signer = BlockSigner::new(key);
let signed = signer.sign(&block).await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
