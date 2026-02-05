//! Integration tests for Go-Rust enclave server interoperability.
//!
//! These tests verify that the Rust implementation is compatible with the Go
//! enclave server, ensuring cross-language crypto and protocol compatibility.
//!
//! # Running Tests
//!
//! These tests require the Go server to be built and running:
//!
//! ```bash
//! # Build the Go server
//! cd op-enclave && go build -o bin/enclave ./cmd/enclave
//!
//! # Start the Go server in local mode
//! OP_ENCLAVE_SIGNER_KEY=ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
//!     ./bin/enclave &
//!
//! # Run integration tests
//! cargo test --package op-enclave-server --test go_interop -- --ignored --test-threads=1
//! ```

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use op_enclave_server::Server;
use op_enclave_server::crypto::{
    decrypt_pkcs1v15, encrypt_pkcs1v15, generate_rsa_key, pkix_to_public_key, private_to_public,
    public_key_to_pkix, signer_from_hex,
};

/// Well-known test signer private key (Hardhat account #0).
const TEST_SIGNER_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

/// Expected Ethereum address for the test signer key.
const TEST_SIGNER_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

/// Port for the Go server.
const GO_SERVER_PORT: u16 = 1234;

/// Go server base URL.
fn go_server_url() -> String {
    format!("http://127.0.0.1:{GO_SERVER_PORT}")
}

/// Handle to a running Go server process.
struct GoServerHandle {
    child: Option<Child>,
}

impl GoServerHandle {
    /// Spawn a new Go server process.
    fn spawn() -> Result<Self, Box<dyn std::error::Error>> {
        // Find the workspace root (where rust/ directory is)
        let cwd = std::env::current_dir()?;

        // Try to find the enclave binary in various locations
        let possible_paths = [
            // From rust directory: ../op-enclave/op-enclave/bin/enclave
            cwd.parent().map(|p| {
                p.join("op-enclave")
                    .join("op-enclave")
                    .join("bin")
                    .join("enclave")
            }),
            // From rust directory: ../op-enclave/bin/enclave (flat structure)
            cwd.parent()
                .map(|p| p.join("op-enclave").join("bin").join("enclave")),
            // Direct path from workspace root
            Some(
                cwd.join("..")
                    .join("op-enclave")
                    .join("op-enclave")
                    .join("bin")
                    .join("enclave"),
            ),
        ];

        // First check if a server is already running
        let client = reqwest::blocking::Client::new();
        if client.post(go_server_url())
            .json(&serde_json::json!({"jsonrpc": "2.0", "method": "enclave_signerPublicKey", "params": [], "id": 1}))
            .send()
            .is_ok()
        {
            // Server already running, don't spawn a new one
            return Ok(Self { child: None });
        }

        let enclave_binary = possible_paths
            .into_iter()
            .flatten()
            .find(|p| p.exists())
            .ok_or_else(|| {
                format!(
                    "Go enclave binary not found. Searched from cwd: {}. Run: cd op-enclave/op-enclave && go build -o bin/enclave ./cmd/enclave",
                    cwd.display()
                )
            })?;

        let child = Command::new(&enclave_binary)
            .env("OP_ENCLAVE_SIGNER_KEY", TEST_SIGNER_KEY)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        // Wait for server to start
        std::thread::sleep(Duration::from_millis(500));

        Ok(Self { child: Some(child) })
    }
}

impl Drop for GoServerHandle {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// JSON-RPC client for the Go enclave server.
struct GoEnclaveClient {
    client: reqwest::blocking::Client,
    url: String,
}

/// JSON-RPC request body.
#[derive(serde::Serialize)]
struct JsonRpcRequest<P: serde::Serialize> {
    jsonrpc: &'static str,
    method: &'static str,
    params: P,
    id: u64,
}

/// JSON-RPC response body.
#[derive(serde::Deserialize)]
struct JsonRpcResponse<R> {
    result: Option<R>,
    error: Option<JsonRpcError>,
    #[allow(dead_code)]
    id: u64,
}

/// JSON-RPC error.
#[derive(serde::Deserialize, Debug)]
struct JsonRpcError {
    code: i64,
    message: String,
}

impl GoEnclaveClient {
    fn new() -> Self {
        Self {
            client: reqwest::blocking::Client::new(),
            url: go_server_url(),
        }
    }

    fn call<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &'static str,
        params: P,
    ) -> Result<R, Box<dyn std::error::Error>> {
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            method,
            params,
            id: 1,
        };

        let response: JsonRpcResponse<R> =
            self.client.post(&self.url).json(&request).send()?.json()?;

        if let Some(error) = response.error {
            return Err(format!("JSON-RPC error {}: {}", error.code, error.message).into());
        }

        response.result.ok_or_else(|| "no result".into())
    }

    /// Get the signer's public key (65-byte uncompressed EC point, hex-encoded).
    fn get_signer_public_key(&self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let result: String = self.call("enclave_signerPublicKey", ())?;
        let bytes = hex::decode(result.trim_start_matches("0x"))?;
        Ok(bytes)
    }

    /// Get the decryption public key (PKIX/DER format, hex-encoded).
    fn get_decryption_public_key(&self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let result: String = self.call("enclave_decryptionPublicKey", ())?;
        let bytes = hex::decode(result.trim_start_matches("0x"))?;
        Ok(bytes)
    }

    /// Set the signer key from an encrypted blob.
    fn set_signer_key(&self, encrypted: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        let hex_encrypted = format!("0x{}", hex::encode(encrypted));
        let _: serde_json::Value = self.call("enclave_setSignerKey", [hex_encrypted])?;
        Ok(())
    }
}

/// Test that both servers return 65-byte uncompressed EC points for signer public key.
#[test]
#[ignore = "requires Go server"]
fn test_signer_public_key_format() {
    let _server = GoServerHandle::spawn().expect("failed to spawn Go server");
    let client = GoEnclaveClient::new();

    // Get Go server's public key
    let go_public_key = client
        .get_signer_public_key()
        .expect("failed to get Go public key");

    // Verify format: 65 bytes, starts with 0x04 (uncompressed point marker)
    assert_eq!(go_public_key.len(), 65, "Go public key should be 65 bytes");
    assert_eq!(
        go_public_key[0], 0x04,
        "Go public key should start with 0x04"
    );

    // Create a Rust server with the same key
    // SAFETY: This test is single-threaded and we control the environment.
    unsafe { std::env::set_var("OP_ENCLAVE_SIGNER_KEY", TEST_SIGNER_KEY) };
    let rust_server = Server::new().expect("failed to create Rust server");
    let rust_public_key = rust_server.signer_public_key();

    // Verify Rust key has same format
    assert_eq!(
        rust_public_key.len(),
        65,
        "Rust public key should be 65 bytes"
    );
    assert_eq!(
        rust_public_key[0], 0x04,
        "Rust public key should start with 0x04"
    );

    // When using the same private key, public keys should match
    assert_eq!(
        go_public_key, rust_public_key,
        "public keys should match when using same private key"
    );

    // Verify address matches expected
    let rust_address = format!("{}", rust_server.signer_address());
    assert_eq!(
        rust_address, TEST_SIGNER_ADDRESS,
        "Rust address should match expected"
    );
}

/// Test that Go's PKIX-encoded decryption key is parseable by Rust.
#[test]
#[ignore = "requires Go server"]
fn test_decryption_public_key_pkix() {
    let _server = GoServerHandle::spawn().expect("failed to spawn Go server");
    let client = GoEnclaveClient::new();

    // Get Go server's decryption public key
    let go_pkix = client
        .get_decryption_public_key()
        .expect("failed to get Go decryption key");

    // PKIX-encoded RSA-4096 key should be ~550 bytes
    assert!(
        go_pkix.len() > 500,
        "PKIX key should be at least 500 bytes, got {}",
        go_pkix.len()
    );

    // Rust should be able to parse it
    let parsed_key = pkix_to_public_key(&go_pkix).expect("Rust should parse Go's PKIX key");

    // Re-encode and verify roundtrip
    let re_encoded = public_key_to_pkix(&parsed_key).expect("should re-encode");
    assert_eq!(go_pkix, re_encoded, "PKIX roundtrip should be identical");
}

/// Test that Rust can encrypt a signer key and Go can decrypt it via setSignerKey.
///
/// NOTE: This test requires actual NSM hardware because Go's `SetSignerKey` uses
/// `nsm.OpenDefaultSession()` for the RSA decryption random source. In local mode,
/// this will fail with "failed to open session: open /dev/nsm: no such file or directory".
/// This test will pass when run inside an actual Nitro Enclave.
#[test]
#[ignore = "requires Go server with NSM hardware"]
fn test_rust_encrypts_go_decrypts() {
    let _server = GoServerHandle::spawn().expect("failed to spawn Go server");
    let client = GoEnclaveClient::new();

    // Get initial signer public key from Go
    let initial_public_key = client
        .get_signer_public_key()
        .expect("failed to get initial public key");

    // Get Go's decryption public key
    let go_decryption_pkix = client
        .get_decryption_public_key()
        .expect("failed to get decryption key");

    // Parse the public key in Rust
    let go_decryption_key =
        pkix_to_public_key(&go_decryption_pkix).expect("failed to parse decryption key");

    // Create a new private key to send
    let new_private_key_hex = "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
    let new_private_key_bytes = hex::decode(new_private_key_hex).expect("valid hex");

    // Encrypt the new key with Go's decryption public key
    let mut rng = rand::rngs::OsRng;
    let encrypted = encrypt_pkcs1v15(&mut rng, &go_decryption_key, &new_private_key_bytes)
        .expect("failed to encrypt");

    // Send to Go server
    client
        .set_signer_key(&encrypted)
        .expect("Go should decrypt Rust-encrypted key");

    // Verify the signer key changed
    let new_public_key = client
        .get_signer_public_key()
        .expect("failed to get new public key");

    assert_ne!(
        initial_public_key, new_public_key,
        "signer public key should have changed"
    );

    // Verify the new key matches what we expect
    let expected_signer =
        signer_from_hex(new_private_key_hex).expect("failed to create expected signer");
    let expected_public_key = op_enclave_server::crypto::public_key_bytes(&expected_signer);

    assert_eq!(
        new_public_key, expected_public_key,
        "new public key should match expected"
    );
}

/// Test RSA encrypt/decrypt roundtrip without server (pure crypto test).
#[test]
fn test_rsa_crypto_compatibility() {
    let mut rng = rand::rngs::OsRng;

    // Generate an RSA key
    let private_key = generate_rsa_key(&mut rng).expect("failed to generate RSA key");
    let public_key = private_to_public(&private_key);

    // Serialize to PKIX (same format Go uses)
    let pkix = public_key_to_pkix(&public_key).expect("failed to serialize to PKIX");

    // Parse it back
    let parsed_key = pkix_to_public_key(&pkix).expect("failed to parse PKIX");
    assert_eq!(public_key, parsed_key, "PKIX roundtrip should preserve key");

    // Encrypt a 32-byte ECDSA private key
    let ecdsa_key = hex::decode(TEST_SIGNER_KEY).expect("valid hex");
    assert_eq!(ecdsa_key.len(), 32, "ECDSA key should be 32 bytes");

    let encrypted = encrypt_pkcs1v15(&mut rng, &public_key, &ecdsa_key).expect("failed to encrypt");

    // Decrypt
    let decrypted = decrypt_pkcs1v15(&private_key, &encrypted).expect("failed to decrypt");

    assert_eq!(ecdsa_key, decrypted, "decrypted key should match original");
}
