//! ERC-8168 payer co-signer.
//!
//! The builder-operated payer is a full-owner secp256k1 EOA (scope `0x00`,
//! the implicit default-EOA admin — see EIP-8130 scopes). It authorizes a
//! sponsored EIP-8130 transaction just-in-time during block construction by
//! signing the transaction's payer digest
//! ([`TxEip8130::payer_signature_hash`], which substitutes the resolved sender)
//! and wrapping the recoverable signature as the canonical k1 `payer_auth`
//! blob: `K1_AUTHENTICATOR(20) || r(32) || s(32) || v(1)`, `v ∈ {27, 28}`.
//!
//! [`PayerDigestSigner`] is the key-backend seam: [`LocalPayerSigner`] holds a
//! local key today, and a remote (KMS/HSM/Web3Signer) backend can implement the
//! same trait without touching [`PayerCosigner`], which owns the digest and
//! blob-encoding logic.

use alloy_primitives::{Address, B256, Bytes};
use base_common_consensus::{Eip8130Constants, Eip8130Signed, TxEip8130};
use k256::ecdsa::SigningKey;

/// Errors produced while co-signing a sponsored EIP-8130 transaction.
#[derive(Debug, thiserror::Error)]
pub enum PayerSignerError {
    /// The provided private key bytes are not a valid secp256k1 scalar.
    #[error("payer private key is invalid")]
    InvalidKey,

    /// The digest signer failed to produce a signature.
    #[error("payer digest signing failed")]
    Signing,

    /// The transaction's `payer` field does not designate this co-signer, so
    /// signing it would produce a `payer_auth` that cannot authorize.
    #[error("transaction payer {expected:?} does not match co-signer {actual}")]
    PayerMismatch {
        /// The `payer` the transaction designates.
        expected: Option<Address>,
        /// This co-signer's payer address.
        actual: Address,
    },
}

/// Signs a 32-byte EIP-8130 payer digest, returning a 65-byte recoverable k1
/// signature (`r || s || v`, `v ∈ {27, 28}`).
pub trait PayerDigestSigner {
    /// The payer account address every signature recovers to.
    fn address(&self) -> Address;

    /// Signs `digest`, returning the 65-byte recoverable signature.
    fn sign_digest(&self, digest: B256) -> Result<[u8; 65], PayerSignerError>;
}

/// A payer key held locally as a secp256k1 signing key.
#[derive(Debug)]
pub struct LocalPayerSigner {
    key: SigningKey,
    address: Address,
}

impl LocalPayerSigner {
    /// Wraps a secp256k1 signing key, deriving its payer address.
    pub fn new(key: SigningKey) -> Self {
        let address = Address::from_public_key(key.verifying_key());
        Self { key, address }
    }

    /// Builds a signer from a 32-byte big-endian private key.
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<Self, PayerSignerError> {
        let key = SigningKey::from_slice(bytes).map_err(|_| PayerSignerError::InvalidKey)?;
        Ok(Self::new(key))
    }
}

impl PayerDigestSigner for LocalPayerSigner {
    fn address(&self) -> Address {
        self.address
    }

    fn sign_digest(&self, digest: B256) -> Result<[u8; 65], PayerSignerError> {
        let (signature, recovery_id) = self
            .key
            .sign_prehash_recoverable(digest.as_slice())
            .map_err(|_| PayerSignerError::Signing)?;
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&signature.to_bytes());
        out[64] = recovery_id.to_byte() + 27;
        Ok(out)
    }
}

/// Co-signs sponsored EIP-8130 transactions as the builder's payer account.
#[derive(Debug)]
pub struct PayerCosigner<S> {
    signer: S,
}

impl<S: PayerDigestSigner> PayerCosigner<S> {
    /// Wraps a [`PayerDigestSigner`] key backend.
    pub const fn new(signer: S) -> Self {
        Self { signer }
    }

    /// The payer account address this co-signer authorizes as.
    pub fn address(&self) -> Address {
        self.signer.address()
    }

    /// Produces the k1 `payer_auth` blob (`K1_AUTHENTICATOR || r || s || v`)
    /// authorizing gas sponsorship for `tx`.
    ///
    /// `resolved_sender` is the transaction's authenticated sender — the
    /// recovered EOA on the `tx.sender == None` path, or `tx.sender` on the
    /// configured-actor path — and is substituted into the payer digest per
    /// EIP-8130. Fails with [`PayerSignerError::PayerMismatch`] unless
    /// `tx.payer` designates this co-signer.
    pub fn payer_auth(
        &self,
        tx: &TxEip8130,
        resolved_sender: Address,
    ) -> Result<Bytes, PayerSignerError> {
        if tx.payer != Some(self.address()) {
            return Err(PayerSignerError::PayerMismatch {
                expected: tx.payer,
                actual: self.address(),
            });
        }
        let signature = self.signer.sign_digest(tx.payer_signature_hash(resolved_sender))?;
        let mut out = Vec::with_capacity(20 + 65);
        out.extend_from_slice(Eip8130Constants::K1_AUTHENTICATOR.as_slice());
        out.extend_from_slice(&signature);
        Ok(Bytes::from(out))
    }

    /// Attaches the co-signed `payer_auth` to `tx` and its existing
    /// `sender_auth`, yielding the fully-authorized [`Eip8130Signed`].
    pub fn cosign(
        &self,
        tx: TxEip8130,
        sender_auth: Bytes,
        resolved_sender: Address,
    ) -> Result<Eip8130Signed, PayerSignerError> {
        let payer_auth = self.payer_auth(&tx, resolved_sender)?;
        Ok(Eip8130Signed::new(tx, sender_auth, payer_auth))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

    use super::*;

    const SENDER: Address = address!("0x00000000000000000000000000000000000000dd");

    fn local_signer() -> LocalPayerSigner {
        LocalPayerSigner::from_bytes(&[0x11; 32]).unwrap()
    }

    /// Recovers the signer address from a 65-byte `r || s || v` signature over
    /// `digest`, mirroring the protocol's k1 recovery.
    fn recover(digest: B256, signature: &[u8]) -> Address {
        let recovery_id = RecoveryId::from_byte(signature[64] - 27).unwrap();
        let sig = Signature::from_slice(&signature[..64]).unwrap();
        let key = VerifyingKey::recover_from_prehash(digest.as_slice(), &sig, recovery_id).unwrap();
        Address::from_public_key(&key)
    }

    #[test]
    fn sign_digest_recovers_to_payer_and_uses_eip155_v() {
        let signer = local_signer();
        let digest = B256::repeat_byte(0xab);
        let signature = signer.sign_digest(digest).unwrap();
        assert!(matches!(signature[64], 27 | 28));
        assert_eq!(recover(digest, &signature), signer.address());
    }

    #[test]
    fn payer_auth_is_k1_wrapped_and_verifies() {
        let cosigner = PayerCosigner::new(local_signer());
        let payer = cosigner.address();
        let tx = TxEip8130 { payer: Some(payer), ..Default::default() };

        let auth = cosigner.payer_auth(&tx, SENDER).unwrap();
        assert_eq!(auth.len(), 85);
        assert_eq!(&auth[..20], Eip8130Constants::K1_AUTHENTICATOR.as_slice());
        // The embedded signature recovers to the payer over the payer digest,
        // which binds the resolved sender.
        assert_eq!(recover(tx.payer_signature_hash(SENDER), &auth[20..]), payer);
    }

    #[test]
    fn payer_auth_binds_resolved_sender() {
        let cosigner = PayerCosigner::new(local_signer());
        let tx = TxEip8130 { payer: Some(cosigner.address()), ..Default::default() };
        let other_sender = address!("0x00000000000000000000000000000000000000ee");
        // A different resolved sender changes the digest, hence the signature.
        assert_ne!(
            cosigner.payer_auth(&tx, SENDER).unwrap(),
            cosigner.payer_auth(&tx, other_sender).unwrap()
        );
    }

    #[test]
    fn cosign_carries_both_auth_blobs() {
        let cosigner = PayerCosigner::new(local_signer());
        let tx = TxEip8130 { payer: Some(cosigner.address()), ..Default::default() };
        let sender_auth = Bytes::from(vec![1, 2, 3]);

        let signed = cosigner.cosign(tx, sender_auth.clone(), SENDER).unwrap();
        assert_eq!(signed.sender_auth(), &sender_auth);
        assert_eq!(signed.payer_auth().len(), 85);
    }

    #[test]
    fn wrong_payer_is_rejected() {
        let cosigner = PayerCosigner::new(local_signer());
        let other = address!("0x00000000000000000000000000000000000000ee");
        let tx = TxEip8130 { payer: Some(other), ..Default::default() };
        assert!(matches!(
            cosigner.payer_auth(&tx, SENDER).unwrap_err(),
            PayerSignerError::PayerMismatch { .. }
        ));
    }
}
