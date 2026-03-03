# `base-enclave-client`

Enclave client implementation.

This crate provides a client for communicating with the enclave RPC server.

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
