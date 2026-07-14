use alloy_primitives::{Address, B256, Bytes, hex};
use base64::{
    Engine,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use serde::Deserialize;

use crate::{Result, TdxAttestationTokenProvider, TdxRuntimeError, TdxSigner};

/// Chain-specific context bound into a signer registration token nonce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TdxAttestationContext {
    /// L1 chain ID for the signer registry.
    pub chain_id: u64,
    /// `TEEProverRegistry` address receiving the signer registration.
    pub registry_address: Address,
}

/// TDX runtime owning signer identity and Confidential Space token collection.
pub struct TdxRuntime {
    signer: TdxSigner,
    token_provider: Box<dyn TdxAttestationTokenProvider>,
    audience: String,
}

impl TdxRuntime {
    /// Creates a runtime with a fresh signer and Confidential Space token provider.
    pub fn new(
        token_provider: impl TdxAttestationTokenProvider + 'static,
        audience: impl Into<String>,
    ) -> Self {
        Self {
            signer: TdxSigner::generate(),
            token_provider: Box::new(token_provider),
            audience: audience.into(),
        }
    }

    /// Returns the signer's public key.
    pub fn signer_public_key(&self) -> Bytes {
        self.signer.public_key()
    }

    /// Signs arbitrary bytes using the TDX signer.
    pub fn sign(&self, data: &[u8]) -> Result<Bytes> {
        self.signer.sign(data)
    }

    /// Requests a fresh Confidential Space PKI token.
    pub fn attestation_token(&self, nonce: Option<B256>) -> Result<Bytes> {
        let nonces = nonce.map_or_else(Vec::new, |nonce| vec![hex::encode(nonce)]);
        self.token_provider.token(&self.audience, &nonces)
    }

    /// Returns the OCI image digest asserted by a fresh Confidential Space token.
    pub fn workload_digest(&self) -> Result<B256> {
        let token = self.attestation_token(None)?;
        Self::workload_digest_from_token(&token)
    }

    /// Reads the OCI image digest claim from a token without validating its signature.
    ///
    /// Signer registration validates the complete token before this value becomes
    /// trusted onchain.
    pub fn workload_digest_from_token(token: &[u8]) -> Result<B256> {
        let token = std::str::from_utf8(token)
            .map_err(|error| TdxRuntimeError::AttestationTokenResponse(error.to_string()))?;
        let claims = token.split('.').nth(1).ok_or_else(|| {
            TdxRuntimeError::AttestationTokenResponse("token has no claims".into())
        })?;
        let claims = URL_SAFE_NO_PAD
            .decode(claims)
            .or_else(|_| URL_SAFE.decode(claims))
            .map_err(|error| TdxRuntimeError::AttestationTokenResponse(error.to_string()))?;
        let claims: TokenClaims = serde_json::from_slice(&claims)
            .map_err(|error| TdxRuntimeError::AttestationTokenResponse(error.to_string()))?;
        let digest =
            claims.submods.container.image_digest.strip_prefix("sha256:").ok_or_else(|| {
                TdxRuntimeError::AttestationTokenResponse(
                    "workload token image digest is not sha256".into(),
                )
            })?;
        let digest = hex::decode(digest)
            .map_err(|error| TdxRuntimeError::AttestationTokenResponse(error.to_string()))?;
        if digest.len() != 32 {
            return Err(TdxRuntimeError::AttestationTokenResponse(
                "workload token image digest is not 32 bytes".into(),
            ));
        }
        Ok(B256::from_slice(&digest))
    }
}

impl std::fmt::Debug for TdxRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TdxRuntime").field("signer", &self.signer).finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct TokenClaims {
    submods: TokenSubmodules,
}

#[derive(Deserialize)]
struct TokenSubmodules {
    container: ContainerClaims,
}

#[derive(Deserialize)]
struct ContainerClaims {
    image_digest: String,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;

    use super::*;

    #[derive(Debug)]
    struct StaticTokenProvider(Bytes);

    impl TdxAttestationTokenProvider for StaticTokenProvider {
        fn token(&self, _audience: &str, _nonces: &[String]) -> Result<Bytes> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn runtime_reads_oci_digest_from_launcher_token() {
        let claims = r#"{"submods":{"container":{"image_digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222"}}}"#;
        let token = format!("header.{}.signature", URL_SAFE_NO_PAD.encode(claims.as_bytes()));
        let runtime = TdxRuntime::new(
            StaticTokenProvider(Bytes::from(token.into_bytes())),
            "base-tdx-prover",
        );

        assert_eq!(runtime.workload_digest().unwrap(), B256::repeat_byte(0x22));
    }
}
