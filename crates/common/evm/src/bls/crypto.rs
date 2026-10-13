//! BLS12-381 point decoding, hash-to-curve and pairing checks for native BLS transactions.

use alloc::vec::Vec;

use alloy_primitives::B256;
use ark_bls12_381::{Bls12_381, G1Affine, G1Projective, G2Affine, g1};
use ark_ec::{
    AffineRepr,
    hashing::{HashToCurve, curve_maps::wb::WBMap, map_to_curve_hasher::MapToCurveBasedHasher},
    pairing::Pairing,
};
use ark_ff::{Zero, field_hashers::DefaultFieldHasher};
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize, Compress, Validate};
use base_common_consensus::BlsConstants;
use sha2::Sha256;

/// BLS12-381 operations for the minimal-signature-size `POP` ciphersuites used by native BLS
/// transactions: public keys in G2 (96-byte compressed), signatures in G1 (48-byte compressed).
///
/// Backed by arkworks so it builds in `no_std` (proof/zkVM) environments and is deterministic
/// across every execution path.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlsCrypto;

impl BlsCrypto {
    /// Decodes a compressed G2 public key, returning `None` unless it is the canonical encoding
    /// of a non-identity point on the curve and in the prime-order subgroup (`KeyValidate`).
    pub fn public_key(bytes: &[u8; BlsConstants::PUBKEY_LEN]) -> Option<G2Affine> {
        Self::decode_point(bytes)
    }

    /// Decodes a compressed G1 signature, returning `None` unless it is the canonical encoding
    /// of a non-identity point on the curve and in the prime-order subgroup.
    pub fn signature(bytes: &[u8; BlsConstants::SIGNATURE_LEN]) -> Option<G1Affine> {
        Self::decode_point(bytes)
    }

    /// Hashes `msg` to G1 under the domain separation tag `dst` with the RFC 9380
    /// `BLS12381G1_XMD:SHA-256_SSWU_RO_` suite. Returns `None` only if the hasher rejects its
    /// input, which cannot happen for the fixed ciphersuite tags.
    pub fn hash_to_g1(dst: &[u8], msg: &[u8]) -> Option<G1Affine> {
        MapToCurveBasedHasher::<
            G1Projective,
            DefaultFieldHasher<Sha256, 128>,
            WBMap<g1::Config>,
        >::new(dst)
        .and_then(|hasher| hasher.hash(msg))
        .ok()
    }

    /// Verifies a proof of possession (`PopVerify`): `pop` must be a signature by `public_key`
    /// over its own canonical compressed encoding under [`BlsConstants::POP_DST`].
    pub fn verify_proof_of_possession(
        public_key_bytes: &[u8; BlsConstants::PUBKEY_LEN],
        public_key: G2Affine,
        pop: G1Affine,
    ) -> bool {
        let Some(message) = Self::hash_to_g1(BlsConstants::POP_DST, public_key_bytes) else {
            return false;
        };
        Self::pairing_check(pop, [(message, public_key)])
    }

    /// Verifies an aggregate signature over distinct messages (`AggregateVerify`): `signature`
    /// must equal the sum of signatures by each public key over its 32-byte digest under
    /// [`BlsConstants::SIGNATURE_DST`].
    pub fn aggregate_verify(signers: &[(G2Affine, B256)], signature: G1Affine) -> bool {
        let mut terms = Vec::with_capacity(signers.len());
        for (public_key, digest) in signers {
            let Some(message) = Self::hash_to_g1(BlsConstants::SIGNATURE_DST, digest.as_slice())
            else {
                return false;
            };
            terms.push((message, *public_key));
        }
        Self::pairing_check(signature, terms)
    }

    /// Checks `e(signature, g2) == ∏ e(message_i, public_key_i)` as one multi-pairing:
    /// `e(signature, -g2) · ∏ e(message_i, public_key_i) == 1`.
    fn pairing_check(
        signature: G1Affine,
        terms: impl IntoIterator<Item = (G1Affine, G2Affine)>,
    ) -> bool {
        let (mut g1, mut g2): (Vec<G1Affine>, Vec<G2Affine>) = terms.into_iter().unzip();
        g1.push(signature);
        g2.push(-G2Affine::generator());
        Bls12_381::multi_pairing(g1, g2).is_zero()
    }

    /// Decodes a compressed point with full validation and rejects the identity and any
    /// encoding that does not re-serialize to the exact input bytes.
    fn decode_point<P, const N: usize>(bytes: &[u8; N]) -> Option<P>
    where
        P: AffineRepr + CanonicalDeserialize + CanonicalSerialize,
    {
        let point = P::deserialize_with_mode(&bytes[..], Compress::Yes, Validate::Yes).ok()?;
        if point.is_zero() {
            return None;
        }
        let mut canonical = [0u8; N];
        point.serialize_compressed(&mut canonical[..]).ok()?;
        (canonical == *bytes).then_some(point)
    }
}
