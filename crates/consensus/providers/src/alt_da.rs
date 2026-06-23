//! HTTP-backed [`AltDaCommitmentResolver`] used by the online node to resolve alt-DA
//! commitments to off-chain batch bytes.

use std::time::Duration;

use alloy_primitives::{Bytes, hex};
use async_trait::async_trait;
use base_consensus_derive::{AltDaCommitmentResolver, AltDaResolverError};
use base_protocol::BLOB_MAX_DATA_SIZE;
use url::Url;

/// Max bytes accepted from a single resolve, matching the DA server's object cap.
const MAX_RESOLVE_BYTES: usize = 8 * BLOB_MAX_DATA_SIZE;

/// Resolves commitments by calling the alt-DA server's `GET /get/0x{commitment}` endpoint.
///
/// This is the std HTTP counterpart of the `no_std` [`AltDaCommitmentResolver`] trait. It is
/// constructed in the node layer so the consensus crates avoid an `infra` dependency.
#[derive(Debug, Clone)]
pub struct HttpAltDaResolver {
    base: Url,
    http: reqwest::Client,
}

impl HttpAltDaResolver {
    /// Build a resolver targeting `server` (e.g. `http://base-da-server:2583`).
    pub fn new(server: Url) -> Result<Self, AltDaResolverError> {
        let mut base = server;
        base.set_query(None);
        base.set_fragment(None);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| AltDaResolverError::Resolve(e.to_string()))?;
        Ok(Self { base, http })
    }
}

#[async_trait]
impl AltDaCommitmentResolver for HttpAltDaResolver {
    async fn resolve(&self, commitment: &[u8]) -> Result<Bytes, AltDaResolverError> {
        let mut url = self.base.clone();
        url.set_path(&format!("get/0x{}", hex::encode(commitment)));

        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| AltDaResolverError::Resolve(e.to_string()))?;

        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(AltDaResolverError::NotFound);
        }
        if !status.is_success() {
            return Err(AltDaResolverError::Resolve(format!(
                "unexpected status {}",
                status.as_u16()
            )));
        }

        // Reject oversized responses up front by Content-Length, then stream chunks with a
        // running cap so a misconfigured or malicious DA server cannot OOM the node by sending
        // an arbitrarily large body. Mirrors `base_alt_da::Client::get`.
        if let Some(len) = resp.content_length()
            && len as usize > MAX_RESOLVE_BYTES
        {
            return Err(AltDaResolverError::Resolve(format!(
                "response too large: content-length {len} (max {MAX_RESOLVE_BYTES})"
            )));
        }

        let mut bytes = Vec::new();
        let mut response = resp;
        while let Some(chunk) =
            response.chunk().await.map_err(|e| AltDaResolverError::Resolve(e.to_string()))?
        {
            if bytes.len() + chunk.len() > MAX_RESOLVE_BYTES {
                return Err(AltDaResolverError::Resolve(format!(
                    "response too large: {} bytes (max {MAX_RESOLVE_BYTES})",
                    bytes.len() + chunk.len()
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Bytes::from(bytes))
    }
}
