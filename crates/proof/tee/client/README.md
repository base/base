# `base-enclave-client`

Enclave client implementation.

## Overview

Provides `EnclaveClient`, an async RPC client for communicating with the `base-enclave-server`.
Supports querying the signer's public key, performing individual stateless block executions via
`execute_stateless`, aggregating proposals via `aggregate`, and the high-level `prove` method
that delegates full proof orchestration to the enclave server.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-enclave-client = { workspace = true }
```

## Example

```ignore
use base_enclave_client::EnclaveClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = EnclaveClient::new("http://127.0.0.1:1234")?;

    let public_key = client.signer_public_key().await?;
    println!("Signer public key: {:?}", public_key);

    Ok(())
}
```

### Proving

The `prove` method sends a `ProofRequest` to the TEE server and receives a
complete `ProofResult` back. Unlike the lower-level `execute_stateless` and
`aggregate` methods which require the caller to orchestrate individual block
executions and aggregation, `prove` delegates all orchestration to the TEE
server.

```ignore
use base_enclave_client::{EnclaveClient, ProofRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = EnclaveClient::new("http://127.0.0.1:1234")?;

    let request = ProofRequest {
        l1_head: Default::default(),
        agreed_l2_head_hash: Default::default(),
        agreed_l2_output_root: Default::default(),
        claimed_l2_output_root: Default::default(),
        claimed_l2_block_number: 42,
    };

    let result = client.prove(request).await?;
    println!("Proof result: {:?}", result);

    Ok(())
}
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
