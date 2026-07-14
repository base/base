//! Google Confidential Space launcher token collection.

use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

use alloy_primitives::{B256, Bytes};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Serialize;

use crate::{Result, TdxRuntimeError};

const DEFAULT_CONFIDENTIAL_SPACE_SOCKET: &str = "/run/container_launcher/teeserver.sock";

/// Custom audience used by Base's Confidential Space TDX prover.
pub const CONFIDENTIAL_SPACE_AUDIENCE: &str = "base-tdx-prover";

/// Provider for Google Cloud Attestation tokens from the Confidential Space launcher.
pub trait TdxAttestationTokenProvider: Send + Sync {
    /// Requests a PKI token for one audience and zero or more nonce strings.
    fn token(&self, audience: &str, nonces: &[String]) -> Result<Bytes>;
}

/// Confidential Space launcher token provider backed by its Unix socket.
#[derive(Debug)]
pub struct ConfidentialSpaceTokenProvider {
    socket_path: PathBuf,
}

/// Fixed token provider for local development and tests.
#[derive(Clone, Debug)]
pub struct StaticTokenProvider {
    token: Bytes,
}

impl StaticTokenProvider {
    /// Creates a provider that returns `token` for every request.
    pub const fn new(token: Bytes) -> Self {
        Self { token }
    }

    /// Creates an unsigned local-development token containing `image_hash`.
    ///
    /// Local development registers the signer through the development registry
    /// path, so this token must never be accepted by the production verifier.
    pub fn for_image_hash(image_hash: B256) -> Self {
        let claims = serde_json::json!({
            "submods": {
                "container": {
                    "image_digest": format!("sha256:{}", alloy_primitives::hex::encode(image_hash))
                }
            }
        });
        let token = format!(
            "local.{}.unsigned",
            URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&claims).expect("local token claims serialize"))
        );
        Self::new(Bytes::from(token.into_bytes()))
    }
}

impl TdxAttestationTokenProvider for StaticTokenProvider {
    fn token(&self, _audience: &str, _nonces: &[String]) -> Result<Bytes> {
        Ok(self.token.clone())
    }
}

impl ConfidentialSpaceTokenProvider {
    /// Creates a provider using the Confidential Space launcher's default socket.
    pub fn new() -> Self {
        Self::at(DEFAULT_CONFIDENTIAL_SPACE_SOCKET)
    }

    /// Creates a provider using `socket_path`.
    pub fn at(socket_path: impl AsRef<Path>) -> Self {
        Self { socket_path: socket_path.as_ref().to_owned() }
    }
}

impl Default for ConfidentialSpaceTokenProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl TdxAttestationTokenProvider for ConfidentialSpaceTokenProvider {
    fn token(&self, audience: &str, nonces: &[String]) -> Result<Bytes> {
        let body = serde_json::to_vec(&TokenRequest { audience, token_type: "PKI", nonces })
            .map_err(|error| TdxRuntimeError::AttestationToken(error.to_string()))?;
        let request = format!(
            "POST /v1/token HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let mut stream = UnixStream::connect(&self.socket_path)
            .map_err(|error| TdxRuntimeError::filesystem_at(&self.socket_path, error))?;
        stream
            .write_all(request.as_bytes())
            .and_then(|()| stream.write_all(&body))
            .map_err(|error| TdxRuntimeError::filesystem_at(&self.socket_path, error))?;

        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .map_err(|error| TdxRuntimeError::filesystem_at(&self.socket_path, error))?;
        let header_end = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|offset| offset + 4)
            .ok_or_else(|| {
                TdxRuntimeError::AttestationTokenResponse("invalid HTTP response".into())
            })?;
        let (head, token) = response.split_at(header_end);
        if !head.starts_with(b"HTTP/1.1 200 ") && !head.starts_with(b"HTTP/1.0 200 ") {
            return Err(TdxRuntimeError::AttestationTokenResponse(
                String::from_utf8_lossy(head).trim().to_owned(),
            ));
        }
        if token.is_empty() {
            return Err(TdxRuntimeError::AttestationTokenResponse("empty token response".into()));
        }
        Ok(Bytes::copy_from_slice(token))
    }
}

#[derive(Serialize)]
struct TokenRequest<'a> {
    audience: &'a str,
    token_type: &'static str,
    nonces: &'a [String],
}
