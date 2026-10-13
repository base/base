//! Test utilities for native BLS transactions.

use alloy_primitives::{B256, FixedBytes, keccak256};
use ark_bls12_381::{Fr, G1Affine, G1Projective, G2Affine};
use ark_ec::{AffineRepr, CurveGroup};
use ark_ff::PrimeField;
use ark_serialize::CanonicalSerialize;
use base_common_consensus::BlsConstants;

use crate::BlsCrypto;

/// A deterministic BLS secret key that produces public keys, proofs of possession and signatures
/// for the native BLS transaction ciphersuites. For tests only: keys derive from a public seed.
#[derive(Debug, Clone, Copy)]
pub struct BlsTestKey {
    /// The secret scalar.
    pub secret: Fr,
}

impl BlsTestKey {
    /// Derives a key from `seed`.
    pub fn from_seed(seed: u64) -> Self {
        Self { secret: Fr::from_be_bytes_mod_order(keccak256(seed.to_be_bytes()).as_slice()) }
    }

    /// Returns the compressed G2 public key.
    pub fn public_key(&self) -> FixedBytes<96> {
        let mut out = FixedBytes::ZERO;
        (G2Affine::generator() * self.secret)
            .into_affine()
            .serialize_compressed(out.as_mut_slice())
            .expect("96-byte buffer fits a compressed G2 point");
        out
    }

    /// Returns the proof of possession over the compressed public key.
    pub fn proof_of_possession(&self) -> FixedBytes<48> {
        self.sign_message(BlsConstants::POP_DST, self.public_key().as_slice())
    }

    /// Signs a transfer signing digest (`BlsTransfer::signing_digest`).
    pub fn sign(&self, digest: B256) -> FixedBytes<48> {
        self.sign_message(BlsConstants::SIGNATURE_DST, digest.as_slice())
    }

    /// Sums compressed G1 signatures into one aggregate signature.
    pub fn aggregate(signatures: &[FixedBytes<48>]) -> FixedBytes<48> {
        let sum: G1Projective = signatures
            .iter()
            .map(|signature| BlsCrypto::signature(&signature.0).expect("valid test signature"))
            .sum();
        Self::compress(sum.into_affine())
    }

    fn sign_message(&self, dst: &[u8], message: &[u8]) -> FixedBytes<48> {
        let point = BlsCrypto::hash_to_g1(dst, message).expect("fixed ciphersuite tag");
        Self::compress((point * self.secret).into_affine())
    }

    fn compress(point: G1Affine) -> FixedBytes<48> {
        let mut out = FixedBytes::ZERO;
        point
            .serialize_compressed(out.as_mut_slice())
            .expect("48-byte buffer fits a compressed G1");
        out
    }
}
