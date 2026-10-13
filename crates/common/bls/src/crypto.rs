//! BLS12-381 point decoding, hash-to-curve, pairing checks and signature aggregation.

use alloc::vec::Vec;

use alloy_primitives::{B256, FixedBytes};
use ark_bls12_381::{Bls12_381, G1Affine, G1Projective, G2Affine, g1};
use ark_ec::{
    AffineRepr, CurveGroup,
    hashing::{HashToCurve, curve_maps::wb::WBMap, map_to_curve_hasher::MapToCurveBasedHasher},
    pairing::Pairing,
};
use ark_ff::{Zero, field_hashers::DefaultFieldHasher};
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize, Compress, Validate};
use sha2::Sha256;

/// Reasons [`BlsCrypto::aggregate_signatures`] refuses to aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BlsAggregationError {
    /// No signatures were supplied.
    #[error("no BLS signatures to aggregate")]
    Empty,
    /// The signature at `index` is not the canonical compressed encoding of a non-identity G1
    /// point in the prime-order subgroup.
    #[error("invalid BLS signature at index {index}")]
    InvalidSignature {
        /// Position of the rejected signature in the input.
        index: usize,
    },
    /// The signatures sum to the point at infinity, which is never a valid signature.
    #[error("BLS signatures aggregate to the point at infinity")]
    Infinity,
}

/// BLS12-381 operations for the minimal-signature-size `POP` ciphersuites used by native BLS
/// transactions: public keys in G2 (96-byte compressed), signatures in G1 (48-byte compressed).
///
/// Backed by arkworks so it builds in `no_std` (proof/zkVM) environments and is deterministic
/// across every execution path.
///
/// Aggregate verification does not require distinct messages, so it is only sound for public keys
/// whose proof of possession ([`Self::verify_proof_of_possession`]) was verified.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlsCrypto;

impl BlsCrypto {
    /// Length of a compressed G2 public key.
    pub const PUBKEY_LEN: usize = 96;

    /// Length of a compressed G1 signature.
    pub const SIGNATURE_LEN: usize = 48;

    /// CFRG ciphersuite used to sign 32-byte transaction digests (signatures in G1, keys in G2).
    pub const SIGNATURE_DST: &'static [u8] = b"BLS_SIG_BLS12381G1_XMD:SHA-256_SSWU_RO_POP_";

    /// CFRG ciphersuite used for the proof of possession over the compressed public key.
    pub const POP_DST: &'static [u8] = b"BLS_POP_BLS12381G1_XMD:SHA-256_SSWU_RO_POP_";

    /// Decodes a compressed G2 public key, returning `None` unless it is the canonical encoding
    /// of a non-identity point on the curve and in the prime-order subgroup (`KeyValidate`).
    pub fn public_key(bytes: &[u8; Self::PUBKEY_LEN]) -> Option<G2Affine> {
        Self::decode_point(bytes)
    }

    /// Decodes a compressed G1 signature, returning `None` unless it is the canonical encoding
    /// of a non-identity point on the curve and in the prime-order subgroup.
    pub fn signature(bytes: &[u8; Self::SIGNATURE_LEN]) -> Option<G1Affine> {
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
    /// over its own canonical compressed encoding under [`Self::POP_DST`].
    pub fn verify_proof_of_possession(
        public_key_bytes: &[u8; Self::PUBKEY_LEN],
        public_key: G2Affine,
        pop: G1Affine,
    ) -> bool {
        let Some(message) = Self::hash_to_g1(Self::POP_DST, public_key_bytes) else {
            return false;
        };
        Self::pairing_check(pop, [(message, public_key)])
    }

    /// Verifies an aggregate signature (`AggregateVerify` without the distinct-message rule):
    /// `signature` must equal the sum of signatures by each public key over its 32-byte digest
    /// under [`Self::SIGNATURE_DST`]. A single signature verifies as a one-signer aggregate.
    pub fn aggregate_verify(signers: &[(G2Affine, B256)], signature: G1Affine) -> bool {
        let mut terms = Vec::with_capacity(signers.len());
        for (public_key, digest) in signers {
            let Some(message) = Self::hash_to_g1(Self::SIGNATURE_DST, digest.as_slice()) else {
                return false;
            };
            terms.push((message, *public_key));
        }
        Self::pairing_check(signature, terms)
    }

    /// Sums compressed G1 signatures into one compressed aggregate signature.
    ///
    /// Each input may itself be an aggregate: aggregation is associative and commutative, so
    /// re-aggregating partial aggregates yields the same signature as aggregating every
    /// individual signature once. Every input must pass [`Self::signature`] (canonical,
    /// non-identity, prime-order subgroup), and a sum at infinity is rejected.
    pub fn aggregate_signatures(
        signatures: &[FixedBytes<{ Self::SIGNATURE_LEN }>],
    ) -> Result<FixedBytes<{ Self::SIGNATURE_LEN }>, BlsAggregationError> {
        if signatures.is_empty() {
            return Err(BlsAggregationError::Empty);
        }
        let mut sum = G1Projective::zero();
        for (index, signature) in signatures.iter().enumerate() {
            sum += Self::signature(&signature.0)
                .ok_or(BlsAggregationError::InvalidSignature { index })?;
        }
        let sum = sum.into_affine();
        if sum.is_zero() {
            return Err(BlsAggregationError::Infinity);
        }
        let mut out = FixedBytes::ZERO;
        sum.serialize_compressed(out.as_mut_slice())
            .expect("48-byte buffer fits a compressed G1 point");
        Ok(out)
    }

    /// Checks `e(signature, g2) == ∏ e(message_i, public_key_i)` as one multi-pairing:
    /// `e(signature, -g2) · ∏ e(message_i, public_key_i) == 1`.
    pub fn pairing_check(
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
    pub fn decode_point<P, const N: usize>(bytes: &[u8; N]) -> Option<P>
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

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, FixedBytes, b256, keccak256};
    use ark_bls12_381::{Fq, Fr, G1Affine, G2Affine};
    use ark_ec::{AffineRepr, CurveGroup};
    use ark_ff::PrimeField;
    use ark_serialize::CanonicalSerialize;

    use super::*;

    const DIGEST_A: B256 =
        b256!("0x00000000000000000000000000000000000000000000000000000000000000aa");
    const DIGEST_B: B256 =
        b256!("0x00000000000000000000000000000000000000000000000000000000000000bb");
    const DIGEST_C: B256 =
        b256!("0x00000000000000000000000000000000000000000000000000000000000000cc");

    fn secret(seed: u64) -> Fr {
        Fr::from_be_bytes_mod_order(keccak256(seed.to_be_bytes()).as_slice())
    }

    fn public_key(seed: u64) -> G2Affine {
        (G2Affine::generator() * secret(seed)).into_affine()
    }

    fn compress(point: G1Affine) -> FixedBytes<48> {
        let mut out = FixedBytes::ZERO;
        point.serialize_compressed(out.as_mut_slice()).unwrap();
        out
    }

    fn sign(seed: u64, digest: B256) -> FixedBytes<48> {
        let message = BlsCrypto::hash_to_g1(BlsCrypto::SIGNATURE_DST, digest.as_slice()).unwrap();
        compress((message * secret(seed)).into_affine())
    }

    fn verifies(signers: &[(u64, B256)], aggregate: FixedBytes<48>) -> bool {
        let signers: Vec<_> =
            signers.iter().map(|(seed, digest)| (public_key(*seed), *digest)).collect();
        BlsCrypto::aggregate_verify(&signers, BlsCrypto::signature(&aggregate.0).unwrap())
    }

    #[test]
    fn reaggregating_partial_aggregates_equals_flat_aggregate() {
        let (a, b, c) = (sign(1, DIGEST_A), sign(2, DIGEST_B), sign(3, DIGEST_C));
        let flat = BlsCrypto::aggregate_signatures(&[a, b, c]).unwrap();
        let left = BlsCrypto::aggregate_signatures(&[a, b]).unwrap();
        let nested = BlsCrypto::aggregate_signatures(&[left, c]).unwrap();
        let reordered = BlsCrypto::aggregate_signatures(&[c, a, b]).unwrap();
        assert_eq!(nested, flat);
        assert_eq!(reordered, flat);
        // A single input aggregates to itself.
        assert_eq!(BlsCrypto::aggregate_signatures(&[a]).unwrap(), a);
        assert!(verifies(&[(1, DIGEST_A), (2, DIGEST_B), (3, DIGEST_C)], nested));
        // The same signer may sign several digests.
        let repeat = BlsCrypto::aggregate_signatures(&[a, sign(1, DIGEST_B)]).unwrap();
        assert!(verifies(&[(1, DIGEST_A), (1, DIGEST_B)], repeat));
    }

    #[test]
    fn aggregate_rejects_wrong_missing_or_extra_messages_and_keys() {
        let aggregate =
            BlsCrypto::aggregate_signatures(&[sign(1, DIGEST_A), sign(2, DIGEST_B)]).unwrap();
        assert!(verifies(&[(1, DIGEST_A), (2, DIGEST_B)], aggregate));
        // Wrong message.
        assert!(!verifies(&[(1, DIGEST_A), (2, DIGEST_C)], aggregate));
        // Messages swapped between signers.
        assert!(!verifies(&[(1, DIGEST_B), (2, DIGEST_A)], aggregate));
        // Wrong key.
        assert!(!verifies(&[(1, DIGEST_A), (3, DIGEST_B)], aggregate));
        // Missing signer / message.
        assert!(!verifies(&[(1, DIGEST_A)], aggregate));
        // Extra signer / message.
        assert!(!verifies(&[(1, DIGEST_A), (2, DIGEST_B), (3, DIGEST_C)], aggregate));
        // Signature under the proof-of-possession tag instead of the signature tag.
        let message = BlsCrypto::hash_to_g1(BlsCrypto::POP_DST, DIGEST_A.as_slice()).unwrap();
        let wrong_dst = compress((message * secret(1)).into_affine());
        assert!(!verifies(&[(1, DIGEST_A)], wrong_dst));
    }

    #[test]
    fn aggregate_rejects_cancellation_infinity_and_invalid_inputs() {
        let a = sign(1, DIGEST_A);
        let negated = compress(-BlsCrypto::signature(&a.0).unwrap());
        assert_eq!(
            BlsCrypto::aggregate_signatures(&[a, negated]),
            Err(BlsAggregationError::Infinity)
        );
        // Cancellation hidden inside a larger aggregate.
        let b = sign(2, DIGEST_B);
        assert_eq!(
            BlsCrypto::aggregate_signatures(&[a, b, negated]).unwrap(),
            b,
            "a non-zero remainder is a valid aggregate"
        );
        assert_eq!(BlsCrypto::aggregate_signatures(&[]), Err(BlsAggregationError::Empty));

        // The encoded point at infinity.
        let mut infinity = FixedBytes::<48>::ZERO;
        infinity[0] = 0xc0;
        assert_eq!(
            BlsCrypto::aggregate_signatures(&[a, infinity]),
            Err(BlsAggregationError::InvalidSignature { index: 1 })
        );

        // A point on the curve outside the prime-order subgroup.
        let off_subgroup = (1u64..)
            .filter_map(|x| G1Affine::get_point_from_x_unchecked(Fq::from(x), true))
            .find(|point| !point.is_in_correct_subgroup_assuming_on_curve())
            .unwrap();
        assert_eq!(
            BlsCrypto::aggregate_signatures(&[compress(off_subgroup), a]),
            Err(BlsAggregationError::InvalidSignature { index: 0 })
        );

        // A non-canonical encoding: compression flag cleared.
        let mut uncompressed_flag = a;
        uncompressed_flag[0] &= 0x7f;
        assert_eq!(
            BlsCrypto::aggregate_signatures(&[uncompressed_flag]),
            Err(BlsAggregationError::InvalidSignature { index: 0 })
        );
    }
}
