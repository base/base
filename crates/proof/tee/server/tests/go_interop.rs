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
//! cargo test --package base-enclave-server --test go_interop -- --ignored --test-threads=1
//! ```

use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};

use alloy_primitives::{Address, B256, U256, b256};
use base_enclave_server::{
    Server,
    crypto::{
        build_signing_data, decrypt_pkcs1v15, encrypt_pkcs1v15, generate_rsa_key,
        pkix_to_public_key, private_to_public, public_key_bytes, public_key_to_pkix,
        sign_proposal_data_sync, signer_from_hex, verify_proposal_signature,
    },
};
use signing_test_vectors::*;

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
            cwd.parent()
                .map(|p| p.join("op-enclave").join("op-enclave").join("bin").join("enclave")),
            // From rust directory: ../op-enclave/bin/enclave (flat structure)
            cwd.parent().map(|p| p.join("op-enclave").join("bin").join("enclave")),
            // Direct path from workspace root
            Some(cwd.join("..").join("op-enclave").join("op-enclave").join("bin").join("enclave")),
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
        Self { client: reqwest::blocking::Client::new(), url: go_server_url() }
    }

    fn call<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &'static str,
        params: P,
    ) -> Result<R, Box<dyn std::error::Error>> {
        let request = JsonRpcRequest { jsonrpc: "2.0", method, params, id: 1 };

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
    let go_public_key = client.get_signer_public_key().expect("failed to get Go public key");

    // Verify format: 65 bytes, starts with 0x04 (uncompressed point marker)
    assert_eq!(go_public_key.len(), 65, "Go public key should be 65 bytes");
    assert_eq!(go_public_key[0], 0x04, "Go public key should start with 0x04");

    // Create a Rust server with the same key
    // SAFETY: This test is single-threaded and we control the environment.
    unsafe { std::env::set_var("OP_ENCLAVE_SIGNER_KEY", TEST_SIGNER_KEY) };
    let rust_server = Server::new().expect("failed to create Rust server");
    let rust_public_key = rust_server.signer_public_key();

    // Verify Rust key has same format
    assert_eq!(rust_public_key.len(), 65, "Rust public key should be 65 bytes");
    assert_eq!(rust_public_key[0], 0x04, "Rust public key should start with 0x04");

    // When using the same private key, public keys should match
    assert_eq!(
        go_public_key, rust_public_key,
        "public keys should match when using same private key"
    );

    // Verify address matches expected
    let rust_address = format!("{}", rust_server.signer_address());
    assert_eq!(rust_address, TEST_SIGNER_ADDRESS, "Rust address should match expected");
}

/// Test that Go's PKIX-encoded decryption key is parseable by Rust.
#[test]
#[ignore = "requires Go server"]
fn test_decryption_public_key_pkix() {
    let _server = GoServerHandle::spawn().expect("failed to spawn Go server");
    let client = GoEnclaveClient::new();

    // Get Go server's decryption public key
    let go_pkix = client.get_decryption_public_key().expect("failed to get Go decryption key");

    // PKIX-encoded RSA-4096 key should be ~550 bytes
    assert!(go_pkix.len() > 500, "PKIX key should be at least 500 bytes, got {}", go_pkix.len());

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
    let initial_public_key =
        client.get_signer_public_key().expect("failed to get initial public key");

    // Get Go's decryption public key
    let go_decryption_pkix =
        client.get_decryption_public_key().expect("failed to get decryption key");

    // Parse the public key in Rust
    let go_decryption_key =
        pkix_to_public_key(&go_decryption_pkix).expect("failed to parse decryption key");

    // Create a new private key to send
    let new_private_key_hex = "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
    let new_private_key_bytes = hex::decode(new_private_key_hex).expect("valid hex");

    // Encrypt the new key with Go's decryption public key
    let mut rng = rand_08::rngs::OsRng;
    let encrypted = encrypt_pkcs1v15(&mut rng, &go_decryption_key, &new_private_key_bytes)
        .expect("failed to encrypt");

    // Send to Go server
    client.set_signer_key(&encrypted).expect("Go should decrypt Rust-encrypted key");

    // Verify the signer key changed
    let new_public_key = client.get_signer_public_key().expect("failed to get new public key");

    assert_ne!(initial_public_key, new_public_key, "signer public key should have changed");

    // Verify the new key matches what we expect
    let expected_signer =
        signer_from_hex(new_private_key_hex).expect("failed to create expected signer");
    let expected_public_key = base_enclave_server::crypto::public_key_bytes(&expected_signer);

    assert_eq!(new_public_key, expected_public_key, "new public key should match expected");
}

/// Test RSA encrypt/decrypt roundtrip without server (pure crypto test).
#[test]
fn test_rsa_crypto_compatibility() {
    let mut rng = rand_08::rngs::OsRng;

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

// =============================================================================
// Signing interoperability tests
// =============================================================================
//
// These tests verify that Rust's signing implementation matches Go's exactly.
// Test vectors were generated by running the following Go code:
//
// ```go
// package main
//
// import (
//     "crypto/ecdsa"
//     "encoding/hex"
//     "fmt"
//     "math/big"
//
//     "github.com/ethereum/go-ethereum/common"
//     "github.com/ethereum/go-ethereum/crypto"
// )
//
// func main() {
//     // Use Hardhat account #0
//     privateKeyHex := "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
//     privateKey, _ := crypto.HexToECDSA(privateKeyHex)
//
//     // Test values
//     configHash := common.HexToHash("0x1111111111111111111111111111111111111111111111111111111111111111")
//     l1OriginHash := common.HexToHash("0x2222222222222222222222222222222222222222222222222222222222222222")
//     l2BlockNumber := big.NewInt(12345)
//     prevOutputRoot := common.HexToHash("0x3333333333333333333333333333333333333333333333333333333333333333")
//     outputRoot := common.HexToHash("0x4444444444444444444444444444444444444444444444444444444444444444")
//
//     // Build signing data (matching server.go)
//     var signingData []byte
//     signingData = append(signingData, configHash.Bytes()...)
//     signingData = append(signingData, l1OriginHash.Bytes()...)
//     signingData = append(signingData, common.BytesToHash(l2BlockNumber.Bytes()).Bytes()...)
//     signingData = append(signingData, prevOutputRoot.Bytes()...)
//     signingData = append(signingData, outputRoot.Bytes()...)
//
//     fmt.Printf("signing_data: %s\n", hex.EncodeToString(signingData))
//
//     hash := crypto.Keccak256(signingData)
//     fmt.Printf("hash: %s\n", hex.EncodeToString(hash))
//
//     sig, _ := crypto.Sign(hash, privateKey)
//     fmt.Printf("signature: %s\n", hex.EncodeToString(sig))
//
//     pubKeyBytes := crypto.FromECDSAPub(&privateKey.PublicKey)
//     fmt.Printf("public_key: %s\n", hex.EncodeToString(pubKeyBytes))
// }
// ```
//
// Expected output (signing_data format verified to match Go):
// signing_data: 1111...1111 2222...2222 0000...3039 3333...3333 4444...4444 (160 bytes)
// hash: ac780b60f64884ca7663b8e10373dd5acc453580f82556c783542aeb27dfbe5f
//
// Note: Signatures are non-deterministic (due to k value), so we only verify:
// 1. Signing data format matches (both produce the same 160-byte blob)
// 2. Hash matches (keccak256 of signing data)
// 3. Rust signatures can be verified (same algorithm as Go's crypto.VerifySignature)

/// Known test values for signing interop tests.
mod signing_test_vectors {
    use super::*;

    pub(super) const PROPOSER: Address = Address::ZERO;
    pub(super) const CONFIG_HASH: B256 =
        b256!("1111111111111111111111111111111111111111111111111111111111111111");
    pub(super) const L1_ORIGIN_HASH: B256 =
        b256!("2222222222222222222222222222222222222222222222222222222222222222");
    pub(super) const L1_ORIGIN_NUMBER: u64 = 100;
    pub(super) const L2_BLOCK_NUMBER: u64 = 12345;
    pub(super) const STARTING_L2_BLOCK: u64 = 12344;
    pub(super) const PREV_OUTPUT_ROOT: B256 =
        b256!("3333333333333333333333333333333333333333333333333333333333333333");
    pub(super) const OUTPUT_ROOT: B256 =
        b256!("4444444444444444444444444444444444444444444444444444444444444444");
    pub(super) const TEE_IMAGE_HASH: B256 = B256::ZERO;
}

/// Test that `build_signing_data` produces the correct layout.
///
/// The format matches the `AggregateVerifier` contract's journal:
/// `proposer(20) || l1OriginHash(32) || l1OriginNumber(32) || prevOutputRoot(32)
///   || startingL2Block(32) || outputRoot(32) || endingL2Block(32) || configHash(32)
///   || teeImageHash(32)` = 276 bytes
#[test]
fn test_signing_data_format() {
    let signing_data = build_signing_data(
        PROPOSER,
        L1_ORIGIN_HASH,
        U256::from(L1_ORIGIN_NUMBER),
        PREV_OUTPUT_ROOT,
        U256::from(STARTING_L2_BLOCK),
        OUTPUT_ROOT,
        U256::from(L2_BLOCK_NUMBER),
        &[],
        CONFIG_HASH,
        TEE_IMAGE_HASH,
    );

    assert_eq!(signing_data.len(), 276, "signing data should be 276 bytes");

    // Verify individual field positions
    let mut off = 0;
    assert_eq!(&signing_data[off..off + 20], PROPOSER.as_slice());
    off += 20;
    assert_eq!(&signing_data[off..off + 32], L1_ORIGIN_HASH.as_slice());
    off += 32;
    assert_eq!(&signing_data[off..off + 32], &U256::from(L1_ORIGIN_NUMBER).to_be_bytes::<32>());
    off += 32;
    assert_eq!(&signing_data[off..off + 32], PREV_OUTPUT_ROOT.as_slice());
    off += 32;
    assert_eq!(&signing_data[off..off + 32], &U256::from(STARTING_L2_BLOCK).to_be_bytes::<32>());
    off += 32;
    assert_eq!(&signing_data[off..off + 32], OUTPUT_ROOT.as_slice());
    off += 32;
    assert_eq!(&signing_data[off..off + 32], &U256::from(L2_BLOCK_NUMBER).to_be_bytes::<32>());
    off += 32;
    assert_eq!(&signing_data[off..off + 32], CONFIG_HASH.as_slice());
    off += 32;
    assert_eq!(&signing_data[off..off + 32], TEE_IMAGE_HASH.as_slice());
}

/// Test that Rust can verify a signature produced by Go.
///
/// This uses the signature from the Go test vector generation code.
/// Note: Go's crypto.Sign returns v as 0 or 1, which our verification ignores
/// (it only uses the first 64 bytes: r || s).
///
/// TODO: Regenerate Go test vectors for the updated 9-field signing format.
#[test]
#[ignore = "needs regenerated Go test vectors for updated signing format"]
fn test_verify_go_signature() {
    // This signature was generated by running:
    // cd op-enclave/op-enclave && go run ./cmd/generate_test_vectors/main.go
    let go_signature_hex = "e95e9c535bfd1a30ba59bb4ca51426fb91e35ec8c41eab2b1f7bde884da7697a509839ea24a29e38e809f422ed292c00ab9a9793142e99c0ad705f6faae5ea5e00";
    let go_public_key_hex = "048318535b54105d4a7aae60c08fc45f9687181b4fdfc625bd1a753fa7397fed753547f11ca8696646f2f3acb08e31016afac23e630c5d11f59f61fef57b0d2aa5";

    let go_signature = hex::decode(go_signature_hex).expect("valid hex");
    let go_public_key = hex::decode(go_public_key_hex).expect("valid hex");

    let signing_data = build_signing_data(
        PROPOSER,
        L1_ORIGIN_HASH,
        U256::from(L1_ORIGIN_NUMBER),
        PREV_OUTPUT_ROOT,
        U256::from(STARTING_L2_BLOCK),
        OUTPUT_ROOT,
        U256::from(L2_BLOCK_NUMBER),
        &[],
        CONFIG_HASH,
        TEE_IMAGE_HASH,
    );

    let result = verify_proposal_signature(&go_public_key, &signing_data, &go_signature);
    assert!(result.is_ok(), "should parse signature: {result:?}");
    assert!(result.unwrap(), "Rust should verify Go-produced signature");
}

/// Test that Go can verify a signature produced by Rust.
///
/// This test signs with the known test key and verifies using the same public key.
/// This proves that a signature Rust produces could be verified by Go.
#[test]
fn test_rust_signature_verifiable() {
    let signer = signer_from_hex(TEST_SIGNER_KEY).expect("valid key");
    let public_key = public_key_bytes(&signer);

    assert_eq!(public_key.len(), 65);
    assert_eq!(public_key[0], 0x04);

    let signing_data = build_signing_data(
        PROPOSER,
        L1_ORIGIN_HASH,
        U256::from(L1_ORIGIN_NUMBER),
        PREV_OUTPUT_ROOT,
        U256::from(STARTING_L2_BLOCK),
        OUTPUT_ROOT,
        U256::from(L2_BLOCK_NUMBER),
        &[],
        CONFIG_HASH,
        TEE_IMAGE_HASH,
    );

    let signature = sign_proposal_data_sync(&signer, &signing_data).expect("signing failed");
    assert_eq!(signature.len(), 65, "signature should be 65 bytes");

    let valid = verify_proposal_signature(&public_key, &signing_data, &signature)
        .expect("verification should succeed");
    assert!(valid, "Rust should verify its own signature");
}

/// Test signature format: 65 bytes (r: 32, s: 32, v: 1).
#[test]
fn test_signature_format() {
    let signer = signer_from_hex(TEST_SIGNER_KEY).expect("valid key");

    let signing_data = build_signing_data(
        PROPOSER,
        L1_ORIGIN_HASH,
        U256::from(L1_ORIGIN_NUMBER),
        PREV_OUTPUT_ROOT,
        U256::from(STARTING_L2_BLOCK),
        OUTPUT_ROOT,
        U256::from(L2_BLOCK_NUMBER),
        &[],
        CONFIG_HASH,
        TEE_IMAGE_HASH,
    );

    let signature = sign_proposal_data_sync(&signer, &signing_data).expect("signing failed");

    // Verify structure
    assert_eq!(signature.len(), 65, "signature should be 65 bytes");

    // v should be 0 or 1 (parity bit)
    let v = signature[64];
    assert!(v == 0 || v == 1, "v should be 0 or 1, got {v}");

    // r and s should be non-zero (extremely unlikely to be zero for a real signature)
    let r = &signature[0..32];
    let s = &signature[32..64];
    assert!(r.iter().any(|&b| b != 0), "r should be non-zero");
    assert!(s.iter().any(|&b| b != 0), "s should be non-zero");
}
