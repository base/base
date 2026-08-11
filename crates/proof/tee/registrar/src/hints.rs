//! Agora / `nitro-validator` P-384 inverse-hint transcript.
//!
//! Walks the onchain verifier's affine schedule, records each modular inverse as
//! a 48-byte BE hint, and delegates point add/double to [`p384`].

use base_proof_tee_nitro_verifier::AttestationReport;
use p384::{
    AffinePoint, FieldElement, NistP384, ProjectivePoint, Scalar, U384,
    ecdsa::Signature,
    elliptic_curve::{
        FieldBytes as EcFieldBytes, Group,
        bigint::ArrayEncoding,
        ff::PrimeField,
        ops::Reduce,
        sec1::{Coordinates, FromEncodedPoint, ToEncodedPoint},
        subtle::ConstantTimeEq,
    },
};
use sha2::{Digest, Sha384};
use x509_parser::prelude::FromDer;

use crate::{
    error::{HintError, HintResult},
    types::{RegistrationHints, RegistrationPlan},
};

type FieldBytes = EcFieldBytes<NistP384>;

const P384_SCALAR_BYTES: usize = 48;
const P384_SIGNATURE_BYTES: usize = 96;
const UNCOMPRESSED_P384_LEN: usize = 97;
/// Low 256 bits set — splits a P-384 scalar into the verifier's high/low limbs.
const MASK_256: U384 = U384::MAX.shr_vartime(128);

/// Entry points for Agora-compatible P-384 inverse-hint generation.
#[derive(Debug, Default, Clone, Copy)]
pub struct P384Hints;

impl P384Hints {
    /// Verifies a P-384 signature while recording inverses in onchain order.
    ///
    /// `signature` / `pub_key` are fixed-width `r‖s` / `x‖y` (96 bytes).
    /// `hash` is at most 48 bytes (left-padded, then reduced mod `n`).
    pub fn collect_verify_hints(
        hash: &[u8],
        signature: &[u8],
        pub_key: &[u8],
    ) -> HintResult<Vec<u8>> {
        if signature.len() != P384_SIGNATURE_BYTES {
            return Err(HintError::Rejected(format!(
                "signature must be {P384_SIGNATURE_BYTES} bytes, got {}",
                signature.len()
            )));
        }
        if pub_key.len() != P384_SIGNATURE_BYTES {
            return Err(HintError::Rejected(format!(
                "pubkey must be {P384_SIGNATURE_BYTES} bytes, got {}",
                pub_key.len()
            )));
        }
        if hash.len() > P384_SCALAR_BYTES {
            return Err(HintError::Rejected(format!(
                "hash must be at most {P384_SCALAR_BYTES} bytes, got {}",
                hash.len()
            )));
        }

        let mut collector = P384HintCollector::default();
        let r = P384HintCollector::scalar_canonical(&signature[..P384_SCALAR_BYTES])?;
        let s = P384HintCollector::scalar_canonical(&signature[P384_SCALAR_BYTES..])?;
        if bool::from(r.is_zero()) || bool::from(s.is_zero()) {
            return Err(HintError::Rejected("signature rejected by scalar bounds".into()));
        }

        let pub_point = P384HintCollector::affine_from_xy(pub_key)?;
        let mut hash_be = [0u8; P384_SCALAR_BYTES];
        hash_be[P384_SCALAR_BYTES - hash.len()..].copy_from_slice(hash);
        let h = Scalar::reduce(U384::from_be_slice(&hash_be));

        // Solidity records `s⁻¹` twice (once per scalar division).
        let scalar1 = h * collector.record_scalar_inv(&s)?;
        let scalar2 = r * collector.record_scalar_inv(&s)?;

        let points = collector.precompute_table(&pub_point)?;
        let result = collector.double_scalar_mul(&points, &scalar1, &scalar2)?;
        let (x, _) = P384HintCollector::affine_xy(&result)?;
        if Scalar::reduce(U384::from_be_byte_array(x.to_repr())) != r {
            return Err(HintError::Rejected("signature verification failed".into()));
        }
        Ok(collector.into_packed())
    }

    /// Collects hints for a certificate signature under `parent_pub_key` (`x‖y`).
    pub fn collect_cert_signature_hints(cert: &[u8], parent_pub_key: &[u8]) -> HintResult<Vec<u8>> {
        let (hash, signature) = Self::parse_cert_signature(cert)?;
        Self::collect_verify_hints(&hash, &signature, parent_pub_key)
    }

    /// Hint streams for every non-root CA, the leaf, and the attestation signature.
    pub fn for_attestation_report(report: &AttestationReport) -> HintResult<RegistrationHints> {
        if report.doc.cabundle.len() < 2 {
            return Err(HintError::Rejected(
                "attestation cabundle must include root plus at least one non-root CA".into(),
            ));
        }
        let certs = report
            .doc
            .cabundle
            .iter()
            .skip(1)
            .map(|c| c.as_ref())
            .chain(std::iter::once(report.doc.certificate.as_ref()));
        Self::hints_for_chain(report.doc.cabundle[0].as_ref(), certs, |leaf_pub| {
            let tbs = report
                .cose
                .sig_structure()
                .map_err(|e| HintError::Rejected(format!("COSE Sig_structure: {e}")))?;
            Self::collect_verify_hints(
                Sha384::digest(&tbs).as_slice(),
                &report.cose.signature,
                leaf_pub,
            )
        })
    }

    /// Hint streams for a registration plan using `root_cert` as trust anchor.
    pub fn for_registration_plan(
        root_cert: &[u8],
        plan: &RegistrationPlan,
    ) -> HintResult<RegistrationHints> {
        if plan.certs.is_empty() {
            return Err(HintError::Rejected(
                "registration plan must include at least one certificate".into(),
            ));
        }
        Self::hints_for_chain(root_cert, plan.certs.iter().map(|c| c.cert.as_slice()), |leaf_pub| {
            Self::collect_verify_hints(
                Sha384::digest(&plan.attestation_tbs).as_slice(),
                &plan.signature,
                leaf_pub,
            )
        })
    }

    /// Returns fixed-width affine P-384 coordinates (`x‖y`) from a certificate.
    pub fn parse_cert_public_key(cert: &[u8]) -> HintResult<Vec<u8>> {
        let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(cert)
            .map_err(|e| HintError::Certificate(format!("parse X.509 certificate: {e}")))?;
        let pk = parsed.public_key().subject_public_key.data.as_ref();
        if pk.len() != UNCOMPRESSED_P384_LEN || pk[0] != 0x04 {
            return Err(HintError::Certificate(format!(
                "certificate public key must be uncompressed P-384, got {} bytes",
                pk.len()
            )));
        }
        let pub_key = pk[1..].to_vec();
        let _ = P384HintCollector::affine_from_xy(&pub_key)?;
        Ok(pub_key)
    }

    /// Returns the SHA-384 TBS hash and fixed-width `r‖s` signature for `cert`.
    pub fn parse_cert_signature(cert: &[u8]) -> HintResult<(Vec<u8>, Vec<u8>)> {
        let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(cert)
            .map_err(|e| HintError::Certificate(format!("parse X.509 certificate: {e}")))?;
        // ecdsa-with-SHA384
        let oid = parsed.signature_algorithm.oid().to_id_string();
        if oid != "1.2.840.10045.4.3.3" {
            return Err(HintError::Certificate(format!(
                "certificate signature algorithm must be ECDSA-SHA384, got {oid}"
            )));
        }
        let signature = Signature::from_der(parsed.signature_value.data.as_ref())
            .map_err(|e| HintError::Certificate(format!("parse ECDSA certificate signature: {e}")))?
            .to_bytes()
            .to_vec();
        Ok((Sha384::digest(parsed.tbs_certificate.as_ref()).to_vec(), signature))
    }

    fn hints_for_chain<'a>(
        root_cert: &[u8],
        certs: impl IntoIterator<Item = &'a [u8]>,
        attestation_hints: impl FnOnce(&[u8]) -> HintResult<Vec<u8>>,
    ) -> HintResult<RegistrationHints> {
        let mut parent_pub = Self::parse_cert_public_key(root_cert)?;
        let mut cert_signature_hints = Vec::new();
        for cert in certs {
            cert_signature_hints.push(Self::collect_cert_signature_hints(cert, &parent_pub)?);
            parent_pub = Self::parse_cert_public_key(cert)?;
        }
        let attestation_hints = attestation_hints(&parent_pub)?;
        Ok(RegistrationHints { cert_signature_hints, attestation_hints })
    }
}

/// Mutable inverse transcript for the onchain P-384 ECDSA affine schedule.
#[derive(Debug, Default)]
pub struct P384HintCollector {
    inverses: Vec<[u8; P384_SCALAR_BYTES]>,
}

impl P384HintCollector {
    /// Packs recorded inverses as concatenated 48-byte big-endian limbs.
    pub fn into_packed(self) -> Vec<u8> {
        self.inverses.concat()
    }

    fn precompute_table(&mut self, pub_point: &AffinePoint) -> HintResult<[AffinePoint; 64]> {
        let mut points = [AffinePoint::IDENTITY; 64];
        points[0x01] = *pub_point;
        points[0x08] = AffinePoint::GENERATOR;
        for i in 0..8 {
            for j in 0..8 {
                if i + j < 2 {
                    continue;
                }
                let to = (i << 3) | j;
                let (from, addend) = if i == 0 {
                    ((i << 3) | (j - 1), *pub_point)
                } else {
                    (((i - 1) << 3) | j, AffinePoint::GENERATOR)
                };
                points[to] = self.add_affine(&points[from], &addend)?;
            }
        }
        Ok(points)
    }

    fn double_scalar_mul(
        &mut self,
        points: &[AffinePoint; 64],
        scalar1: &Scalar,
        scalar2: &Scalar,
    ) -> HintResult<AffinePoint> {
        let s1 = U384::from(scalar1);
        let s2 = U384::from(scalar2);
        let (s1_hi, s2_hi) = (s1.shr_vartime(256), s2.shr_vartime(256));
        let (s1_lo, s2_lo) = (s1.bitand(&MASK_256), s2.bitand(&MASK_256));

        let mut point = self.twice_affine(&AffinePoint::IDENTITY)?;
        let mut mask = Self::window_bits(&s1_hi, &s2_hi, 183);
        if mask != 0 {
            point = self.add_affine(&points[mask], &point)?;
        }
        for word in (4..=184).step_by(3) {
            point = self.twice3_affine(&point)?;
            mask = Self::window3(&s1_hi, &s2_hi, 184 - word);
            if mask != 0 {
                point = self.add_affine(&points[mask], &point)?;
            }
        }

        point = self.twice_affine(&point)?;
        mask = Self::window_bits(&s1_lo, &s2_lo, 255);
        if mask != 0 {
            point = self.add_affine(&points[mask], &point)?;
        }
        for word in (4..=256).step_by(3) {
            point = self.twice3_affine(&point)?;
            mask = Self::window3(&s1_lo, &s2_lo, 256 - word);
            if mask != 0 {
                point = self.add_affine(&points[mask], &point)?;
            }
        }
        Ok(point)
    }

    fn twice_affine(&mut self, point: &AffinePoint) -> HintResult<AffinePoint> {
        if bool::from(point.is_identity()) {
            return Ok(AffinePoint::IDENTITY);
        }
        let (_, y) = Self::affine_xy(point)?;
        if bool::from(y.is_zero()) {
            return Ok(AffinePoint::IDENTITY);
        }
        self.record_field_inv(&(y + y))?;
        Ok(ProjectivePoint::from(*point).double().to_affine())
    }

    fn twice3_affine(&mut self, point: &AffinePoint) -> HintResult<AffinePoint> {
        let p = self.twice_affine(point)?;
        let p = self.twice_affine(&p)?;
        self.twice_affine(&p)
    }

    fn add_affine(&mut self, a: &AffinePoint, b: &AffinePoint) -> HintResult<AffinePoint> {
        if bool::from(a.is_identity()) {
            return Ok(*b);
        }
        if bool::from(b.is_identity()) {
            return Ok(*a);
        }
        let (x1, y1) = Self::affine_xy(a)?;
        let (x2, y2) = Self::affine_xy(b)?;
        if bool::from(x1.ct_eq(&x2)) {
            return if bool::from(y1.ct_eq(&y2)) {
                self.twice_affine(a)
            } else {
                Ok(AffinePoint::IDENTITY)
            };
        }
        self.record_field_inv(&(x1 - x2))?;
        Ok((ProjectivePoint::from(*a) + ProjectivePoint::from(*b)).to_affine())
    }

    fn record_field_inv(&mut self, value: &FieldElement) -> HintResult<()> {
        let inv: FieldElement = Option::from(FieldElement::invert(value))
            .ok_or_else(|| HintError::Rejected("cannot invert zero".into()))?;
        self.inverses.push(inv.to_repr().as_slice().try_into().unwrap());
        Ok(())
    }

    fn record_scalar_inv(&mut self, value: &Scalar) -> HintResult<Scalar> {
        let inv: Scalar = Option::from(Scalar::invert(value))
            .ok_or_else(|| HintError::Rejected("cannot invert zero".into()))?;
        self.inverses.push(inv.to_repr().as_slice().try_into().unwrap());
        Ok(inv)
    }

    fn affine_from_xy(pub_key: &[u8]) -> HintResult<AffinePoint> {
        let x = Self::field_canonical(&pub_key[..P384_SCALAR_BYTES])?;
        let y = Self::field_canonical(&pub_key[P384_SCALAR_BYTES..])?;
        if bool::from(x.is_zero()) || bool::from(y.is_zero()) {
            return Err(HintError::Rejected("pubkey is not on P-384".into()));
        }
        let mut enc = [0u8; UNCOMPRESSED_P384_LEN];
        enc[0] = 0x04;
        enc[1..].copy_from_slice(pub_key);
        let encoded = p384::EncodedPoint::from_bytes(enc)
            .map_err(|_| HintError::Rejected("invalid P-384 point encoding".into()))?;
        Option::from(AffinePoint::from_encoded_point(&encoded))
            .ok_or_else(|| HintError::Rejected("pubkey is not on P-384".into()))
    }

    fn affine_xy(point: &AffinePoint) -> HintResult<(FieldElement, FieldElement)> {
        let encoded = point.to_encoded_point(false);
        let Coordinates::Uncompressed { x, y } = encoded.coordinates() else {
            return Err(HintError::Rejected("expected uncompressed P-384 point".into()));
        };
        Ok((
            Option::from(FieldElement::from_bytes(x))
                .ok_or_else(|| HintError::Rejected("invalid affine x".into()))?,
            Option::from(FieldElement::from_bytes(y))
                .ok_or_else(|| HintError::Rejected("invalid affine y".into()))?,
        ))
    }

    fn scalar_canonical(bytes: &[u8]) -> HintResult<Scalar> {
        Option::from(Scalar::from_repr(*FieldBytes::from_slice(bytes)))
            .ok_or_else(|| HintError::Rejected("signature rejected by scalar bounds".into()))
    }

    fn field_canonical(bytes: &[u8]) -> HintResult<FieldElement> {
        Option::from(FieldElement::from_bytes(FieldBytes::from_slice(bytes)))
            .ok_or_else(|| HintError::Rejected("pubkey is not on P-384".into()))
    }

    const fn window3(s1: &U384, s2: &U384, shift: usize) -> usize {
        Self::window_bits(
            &s1.shr_vartime(shift).bitand(&U384::from_u64(7)),
            &s2.shr_vartime(shift).bitand(&U384::from_u64(7)),
            0,
        )
    }

    const fn window_bits(s1: &U384, s2: &U384, shift: usize) -> usize {
        let b1 = s1.shr_vartime(shift).as_limbs()[0].0 as usize;
        let b2 = s2.shr_vartime(shift).as_limbs()[0].0 as usize;
        (b1 << 3) | b2
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;
    use p384::{
        ecdsa::{SigningKey, signature::hazmat::PrehashSigner},
        elliptic_curve::Curve,
    };
    use sha2::{Digest, Sha384};

    use super::*;

    /// Shared with `planner` tests — one real Nitro document is enough for golden parity.
    fn fixture_hints() -> RegistrationHints {
        let attestation =
            hex::decode(include_str!("testdata/nitro_attestation.hex").trim()).unwrap();
        let report = AttestationReport::parse(&attestation).unwrap();
        P384Hints::for_attestation_report(&report).unwrap()
    }

    #[test]
    fn nitro_fixture_hint_lengths_and_hashes() {
        let hints = fixture_hints();
        const CERT_LENS: [usize; 4] = [27120, 27456, 27168, 27408];
        const CERT_KECCAK: [&str; 4] = [
            "0xee4279867cda2564a8f129cd98b5e785d9008c83ae4957d08bdc1dddc9ee697f",
            "0xdb39b01ca34a9b1e5e3c21028698a9f8d50a11e40ace71cf7a3867a16b545316",
            "0xdc2f389c144647751c42c7454744f604dd0c89beb5aa271bf7a67d8188cb3187",
            "0x51bc463dbc9dae38959f20166d74a74fe81016830e7867b39595f4589709b282",
        ];
        assert_eq!(hints.cert_signature_hints.len(), CERT_LENS.len());
        for (i, stream) in hints.cert_signature_hints.iter().enumerate() {
            assert_eq!(stream.len(), CERT_LENS[i]);
            assert_eq!(format!("{}", keccak256(stream)), CERT_KECCAK[i]);
        }
        assert_eq!(hints.attestation_hints.len(), 27072);
        assert_eq!(
            format!("{}", keccak256(&hints.attestation_hints)),
            "0x6086b00c3a29f1170bc436fabe95b95557d3f134d4adb56324761cd81998808e"
        );
        assert_eq!(hints, fixture_hints());
    }

    #[test]
    fn collect_verify_hints_rejects_invalid_inputs() {
        let valid_sig = {
            let mut s = vec![0u8; P384_SIGNATURE_BYTES];
            s[P384_SCALAR_BYTES - 1] = 1;
            s[P384_SIGNATURE_BYTES - 1] = 1;
            s
        };
        let zero_r = {
            let mut s = valid_sig.clone();
            s[..P384_SCALAR_BYTES].fill(0);
            s
        };
        let order_r = {
            let mut s = valid_sig.clone();
            s[..P384_SCALAR_BYTES].copy_from_slice(NistP384::ORDER.to_be_byte_array().as_slice());
            s
        };
        let pub_key = [0u8; P384_SIGNATURE_BYTES];
        for (name, hash, signature, key, want) in [
            (
                "bad signature length",
                &[1][..],
                &valid_sig[..95],
                &pub_key[..],
                "signature must be 96 bytes",
            ),
            (
                "bad pubkey length",
                &[1][..],
                &valid_sig[..],
                &pub_key[..95],
                "pubkey must be 96 bytes",
            ),
            (
                "oversized hash",
                &[1u8; 49][..],
                &valid_sig[..],
                &pub_key[..],
                "hash must be at most 48 bytes",
            ),
            ("zero r", &[1][..], &zero_r[..], &pub_key[..], "signature rejected by scalar bounds"),
            (
                "r equal to curve order",
                &[1][..],
                &order_r[..],
                &pub_key[..],
                "signature rejected by scalar bounds",
            ),
            (
                "pubkey not on curve",
                &[1][..],
                &valid_sig[..],
                &pub_key[..],
                "pubkey is not on P-384",
            ),
        ] {
            let err = P384Hints::collect_verify_hints(hash, signature, key).unwrap_err();
            assert!(err.to_string().contains(want), "{name}: got {err}");
        }

        let signing_key = SigningKey::from_slice(&[7u8; 48]).unwrap();
        let signature: Signature =
            signing_key.sign_prehash(Sha384::digest(b"attested payload").as_slice()).unwrap();
        let pub_key = signing_key.verifying_key().to_encoded_point(false).as_bytes()[1..].to_vec();
        let err = P384Hints::collect_verify_hints(
            Sha384::digest(b"a different payload").as_slice(),
            signature.to_bytes().as_slice(),
            &pub_key,
        )
        .unwrap_err();
        assert!(err.to_string().contains("signature verification failed"));
    }
}
