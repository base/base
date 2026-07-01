use std::fmt;

use alloy_primitives::{Address, Bytes, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;

use crate::{Result, TdxRuntimeError};

/// TDX guest secp256k1 signer.
pub struct TdxSigner {
    signer: PrivateKeySigner,
}

impl TdxSigner {
    /// Generates a fresh signer key using OS randomness inside the TDX guest.
    pub fn generate() -> Self {
        Self { signer: PrivateKeySigner::random() }
    }

    /// Returns the uncompressed 65-byte public key (`0x04 || x || y`).
    pub fn public_key(&self) -> Bytes {
        let verifying_key = self.signer.credential().verifying_key();
        let encoded_point = verifying_key.to_encoded_point(false);
        Bytes::copy_from_slice(encoded_point.as_bytes())
    }

    /// Returns the signer's Ethereum address.
    pub const fn address(&self) -> Address {
        self.signer.address()
    }

    /// Signs arbitrary bytes using the Nitro-compatible proof-journal scheme.
    pub fn sign(&self, data: &[u8]) -> Result<Bytes> {
        let hash = keccak256(data);
        let signature = self
            .signer
            .sign_hash_sync(&hash)
            .map_err(|error| TdxRuntimeError::Signing(error.to_string()))?;

        Ok(Bytes::from(signature.as_rsy().to_vec()))
    }
}

impl fmt::Debug for TdxSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TdxSigner").field("address", &self.address()).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_signer_has_uncompressed_public_key() {
        let signer = TdxSigner::generate();
        let public_key = signer.public_key();

        assert_eq!(public_key.len(), 65);
        assert_eq!(public_key[0], 0x04);
    }

    #[test]
    fn signer_debug_does_not_expose_private_key_material() {
        let signer = TdxSigner::generate();
        let debug = format!("{signer:?}");

        assert!(debug.contains("TdxSigner"));
        assert!(debug.contains("address"));
        assert!(!debug.contains("signer:"));
    }

    #[test]
    fn signer_produces_65_byte_signature() {
        let signer = TdxSigner::generate();
        let signature = signer.sign(b"proof journal bytes").unwrap();

        assert_eq!(signature.len(), 65);
    }
}
