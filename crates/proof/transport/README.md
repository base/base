# `base-proof-transport`

Length-prefixed bincode frame codec for proof transport.

## Overview

Provides `Frame`, a simple length-prefixed bincode codec, and `TransportError` for use
by proof transport implementations. The actual transport implementations (vsock, local)
live in the backend-specific crates (e.g. `base-proof-tee-nitro`).

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-proof-transport = { workspace = true, features = ["frame"] }
```

```rust,ignore
use base_proof_transport::Frame;

Frame::write(&mut stream, &my_request).await?;
let response: MyResponse = Frame::read(&mut stream).await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
