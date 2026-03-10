# `base-enclave-server`

HTTP-to-vsock proxy and transport configuration for AWS Nitro Enclaves.

## Overview

Provides the host-side proxy that bridges HTTP traffic to the enclave's vsock interface,
along with transport configuration constants (ports, body limits).

Key components:

- **Proxy**: HTTP-to-vsock reverse proxy for forwarding requests to the enclave
- **Transport**: Vsock and HTTP transport configuration

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-enclave-server = { workspace = true }
```

```rust,ignore
use base_enclave_server::run_proxy;

run_proxy(vsock_cid, vsock_port, http_port).await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
