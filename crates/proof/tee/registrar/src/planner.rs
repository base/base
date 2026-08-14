//! Nitro attestation registration-plan construction.
//!
//! Strictly parses `COSE_Sign1` (raw protected/payload TLVs for TBS) and builds
//! `CertManager`-oriented plan fields required by hinted registration.

use alloy_primitives::{Address, B256, b256, keccak256};
use base_proof_tee_nitro_verifier::{AttestationDocument, AttestationVerifier};
use k256::ecdsa::VerifyingKey;
use x509_parser::prelude::{FromDer, X509Version};

use crate::{
    cbor::NitroCose,
    error::{PlannerError, PlannerResult},
    types::{CertKind, CertPlan, RegistrationPlan},
};

const UNCOMPRESSED_SECP256K1_LEN: usize = 65;

/// Full-DER hash of the pinned AWS Nitro root certificate.
pub const PINNED_ROOT_CERT_HASH: B256 =
    b256!("0x311d96fcd5c5e0ccf72ef548e2ea7d4c0cd53ad7c4cc49e67471aed41d61f185");

/// Parses AWS Nitro `COSE_Sign1` attestations into certificate registration plans.
#[derive(Debug, Default)]
pub struct AttestationPlanner;

impl AttestationPlanner {
    /// Parses a raw `COSE_Sign1` Nitro attestation and builds a registration plan.
    ///
    /// Accepts only immutable attestation bytes (no decoded-report mutation path).
    /// Does not generate P-384 inverse hints or submit transactions. The signer is
    /// derived from attestation `public_key` (Base semantics), never from `user_data`.
    pub fn prepare_registration_plan(attestation: &[u8]) -> PlannerResult<RegistrationPlan> {
        let cose = NitroCose::parse_sign1(attestation)?;
        // Intentional overlap with `AttestationVerifier::validate_attestation_content` below:
        // raw CBOR rejects duplicates/trailing bytes that deserialization would collapse or
        // miss; the shared verifier then re-checks cabundle/PCR/size limits on the decoded doc.
        NitroCose::validate_payload_structure(&cose.payload)?;

        let doc: AttestationDocument = ciborium::de::from_reader(cose.payload.as_slice())
            .map_err(|e| PlannerError::Attestation(format!("attestation document decode: {e}")))?;
        AttestationVerifier::validate_attestation_content(&doc)?;

        let public_key = doc.public_key.as_ref().ok_or_else(|| {
            PlannerError::Attestation("attestation payload missing public_key".into())
        })?;
        let signer = Self::signer_from_public_key(public_key)?;

        let pcr0 = doc
            .pcrs
            .get(&0)
            .ok_or_else(|| PlannerError::Attestation("attestation payload missing PCR0".into()))?
            .as_slice()
            .to_vec();

        if doc.cabundle.len() < 2 {
            return Err(PlannerError::Attestation(
                "attestation cabundle must include root plus at least one non-root CA".into(),
            ));
        }

        let root_cert = doc.cabundle[0].as_ref().to_vec();
        let root_hash = keccak256(&root_cert);
        if root_hash != PINNED_ROOT_CERT_HASH {
            return Err(PlannerError::Attestation(format!(
                "attestation root certificate hash {root_hash} is not trusted"
            )));
        }

        // Capacity = non-root CAs (`cabundle.len() - 1`) + leaf.
        let mut parent_hash = root_hash;
        let mut certs = Vec::with_capacity(doc.cabundle.len());
        for (i, cert) in doc.cabundle.iter().enumerate().skip(1) {
            let cert = cert.as_ref();
            let (cert_hash, revocation_id) = CertManagerKeys::keys(cert)?;
            certs.push(CertPlan {
                kind: CertKind::Ca,
                label: Self::ca_label(i),
                cert: cert.to_vec(),
                cert_hash,
                parent_cert_hash: parent_hash,
                revocation_id,
            });
            parent_hash = cert_hash;
        }

        let leaf = doc.certificate.as_ref();
        let (leaf_hash, leaf_revocation_id) = CertManagerKeys::keys(leaf)?;
        certs.push(CertPlan {
            kind: CertKind::Leaf,
            label: "client / leaf cert".into(),
            cert: leaf.to_vec(),
            cert_hash: leaf_hash,
            parent_cert_hash: parent_hash,
            revocation_id: leaf_revocation_id,
        });

        Ok(RegistrationPlan {
            signer,
            pcr0,
            timestamp: doc.timestamp,
            nonce: doc.nonce.as_ref().map(|n| n.to_vec()),
            root_cert_hash: root_hash,
            root_cert,
            leaf_cert_hash: leaf_hash,
            attestation_tbs: cose.attestation_tbs,
            signature: cose.signature,
            certs,
        })
    }

    /// Human-readable role label for a non-root CA index in the cabundle.
    pub fn ca_label(index: usize) -> String {
        match index {
            1 => "regional CA".into(),
            2 => "zonal CA".into(),
            3 => "issuer / instance CA".into(),
            _ => format!("non-root CA {index}"),
        }
    }

    /// Derives the Base signer address from an uncompressed secp256k1 `public_key`.
    pub fn signer_from_public_key(public_key: &[u8]) -> PlannerResult<Address> {
        if public_key.len() != UNCOMPRESSED_SECP256K1_LEN || public_key[0] != 0x04 {
            return Err(PlannerError::PublicKey(format!(
                "public_key must be a 65-byte uncompressed secp256k1 key, got {} bytes",
                public_key.len()
            )));
        }
        // Validate the key is a real secp256k1 point, then match
        // alloy_signer::utils::public_key_to_address: keccak of uncompressed without 0x04.
        VerifyingKey::from_sec1_bytes(public_key)
            .map_err(|e| PlannerError::PublicKey(format!("invalid secp256k1 public_key: {e}")))?;
        let digest = keccak256(&public_key[1..]);
        Ok(Address::from_slice(&digest[12..]))
    }
}

/// Certificate hash / revocation helpers aligned with onchain `CertManager`.
#[derive(Debug, Default)]
pub struct CertManagerKeys;

impl CertManagerKeys {
    /// Returns `(cache_key, revocation_id)` for `cert` from a single X.509 parse.
    ///
    /// Root certificates use `keccak256(full DER)` as the cache key. Every non-root
    /// certificate uses `keccak256(TBSCertificate DER TLV)` including tag and length.
    /// `revocation_id` is always `keccak256(issuerHash || serialHash)`.
    ///
    /// Rejects trailing DER bytes and requires the version-present v3 TBS layout
    /// expected by `CertManager`.
    pub fn keys(cert: &[u8]) -> PlannerResult<(B256, B256)> {
        let full_hash = keccak256(cert);
        let (remaining, parsed) = x509_parser::certificate::X509Certificate::from_der(cert)
            .map_err(|e| PlannerError::Certificate(Box::new(e)))?;
        if !remaining.is_empty() {
            return Err(Self::cert_error("certificate has trailing DER data"));
        }
        Self::require_v3_layout(parsed.tbs_certificate.as_ref(), parsed.version())?;

        let cache_key = if full_hash == PINNED_ROOT_CERT_HASH {
            full_hash
        } else {
            keccak256(parsed.tbs_certificate.as_ref())
        };

        // `raw_serial()` is the ASN.1 INTEGER content octets, including a leading `0x00` when
        // DER sign-extends a high-bit serial. `CertManager._certIdentity` hashes the same bytes
        // (`serialPtr.content()` / `length()`), so do not strip padding.
        let serial_hash = keccak256(parsed.tbs_certificate.raw_serial());
        let issuer_content = Self::der_content_octets(parsed.tbs_certificate.issuer().as_raw())?;
        let issuer_hash = keccak256(issuer_content);
        let mut material = [0u8; 64];
        material[..32].copy_from_slice(issuer_hash.as_slice());
        material[32..].copy_from_slice(serial_hash.as_slice());
        Ok((cache_key, keccak256(material)))
    }

    /// Returns the verifier cache key for `cert`.
    pub fn cache_key(cert: &[u8]) -> PlannerResult<B256> {
        Ok(Self::keys(cert)?.0)
    }

    /// Returns `CertManager.computeCertId`: `keccak256(issuerHash || serialHash)` where each
    /// component hashes the ASN.1 content octets (excluding tag and length).
    pub fn revocation_id(cert: &[u8]) -> PlannerResult<B256> {
        Ok(Self::keys(cert)?.1)
    }

    /// Requires explicit context-tagged version present with value 2 (X.509 v3).
    ///
    /// The INTEGER child must consume the entire `[0] EXPLICIT` wrapper, matching
    /// `CertManager._parseTbs` (e.g. `a0050201020500` is rejected).
    pub fn require_v3_layout(tbs_tlv: &[u8], version: X509Version) -> PlannerResult<()> {
        let tbs_content = Self::der_content_octets(tbs_tlv)?;
        if tbs_content.first().copied() != Some(0xa0) {
            return Err(Self::cert_error(
                "TBSCertificate missing explicit version context tag (0xa0)",
            ));
        }
        let (wrapper_content, _) = Self::der_tlv_at(tbs_content, 0)?;
        let (_, version_child_end) = Self::der_tlv_at(wrapper_content, 0)?;
        if version_child_end != wrapper_content.len() {
            return Err(Self::cert_error("TBSCertificate version wrapper has trailing data"));
        }
        if version != X509Version::V3 {
            return Err(Self::cert_error("certificate version must be v3"));
        }
        Ok(())
    }

    /// Strips the DER tag and length from an exact-fit TLV, returning content octets.
    ///
    /// # Preconditions
    ///
    /// `tlv` must be exactly one DER TLV: the decoded header+content length must equal
    /// `tlv.len()`. Concatenated TLVs or trailing bytes return `DER TLV length mismatch`.
    pub fn der_content_octets(tlv: &[u8]) -> PlannerResult<&[u8]> {
        let (content, end) = Self::der_tlv_at(tlv, 0)?;
        if end != tlv.len() {
            return Err(Self::cert_error("DER TLV length mismatch"));
        }
        Ok(content)
    }

    /// Parses one DER TLV at `start`. Returns `(content, end)` where `end` may leave siblings.
    pub fn der_tlv_at(bytes: &[u8], start: usize) -> PlannerResult<(&[u8], usize)> {
        if bytes.len().saturating_sub(start) < 2 {
            return Err(Self::cert_error("DER TLV too short"));
        }
        let first = bytes[start + 1];
        let (header_len, content_len) = if first & 0x80 == 0 {
            (2usize, first as usize)
        } else {
            let nbytes = (first & 0x7f) as usize;
            if nbytes == 0 || nbytes > 4 || bytes.len() < start + 2 + nbytes {
                return Err(Self::cert_error("invalid DER length"));
            }
            let mut len = 0usize;
            for b in &bytes[start + 2..start + 2 + nbytes] {
                len = (len << 8) | usize::from(*b);
            }
            (2 + nbytes, len)
        };
        let content_start =
            start.checked_add(header_len).ok_or_else(|| Self::cert_error("DER length overflow"))?;
        let end = content_start
            .checked_add(content_len)
            .ok_or_else(|| Self::cert_error("DER length overflow"))?;
        if end > bytes.len() {
            return Err(Self::cert_error("DER TLV length mismatch"));
        }
        Ok((&bytes[content_start..end], end))
    }

    /// Builds a `PlannerError::Certificate` from a static message.
    pub fn cert_error(message: &'static str) -> PlannerError {
        PlannerError::Certificate(Box::new(std::io::Error::other(message)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cbor::NitroCose;
    use alloy_primitives::{address, b256};

    /// Uncompressed secp256k1 `public_key` embedded in the Base `NitroValidator` fixture.
    const FIXTURE_PUBLIC_KEY: [u8; 65] = hex_literal::hex!(
        "0433a4701fa871b188983d570e2c2d8cf98fd66eb19ba8ca7617bc8e20e152a5d7f0205eae76e608ce855077e4565be69db4471ef72857253742f9602c11ff04e5"
    );
    const FIXTURE_SIGNER: Address = address!("0x3c32C4Ad111a12D7d3Af032cfFAff7789dfa555f");
    const FIXTURE_TBS_KECCAK: B256 =
        b256!("0x59be5f503462c721f5cfc602b612d9f633496681e429b172605f966458abd8e1");

    // CertManager vectors for root / first CA / leaf on the NitroValidator.t.sol chain.
    const REGIONAL_CA_HASH: B256 =
        b256!("0x1bc41a5b307f0e8e2c7a80bbc3b3a9198251c1343a34ab9bc188d351c9fb87b3");
    const REGIONAL_CA_REVOCATION_ID: B256 =
        b256!("0xd985a3a751ddd841816eb3d64041272eed9b695a2d61a46408a1950c0bae28e7");
    const LEAF_HASH: B256 =
        b256!("0xa5853761df0df035211f36e112b8fd2310470a8900e99571f484a29cd30379fb");
    const LEAF_REVOCATION_ID: B256 =
        b256!("0x21da5f8225989c43b293c87136a4ed8260f0508197467cb2484191d002f13c19");

    /// `NitroValidator.t.sol` `test_DecodeAttestationTbs` fixture (Base-shaped: has `public_key`).
    fn base_fixture_attestation() -> Vec<u8> {
        hex::decode(include_str!("testdata/nitro_attestation.hex").trim()).unwrap()
    }

    #[test]
    fn prepare_registration_plan_from_base_fixture_pins_literals() {
        let attestation = base_fixture_attestation();

        // Strict COSE rejects trailing bytes that `AttestationReport::parse` accepts.
        let mut trailing = attestation.clone();
        trailing.push(0x00);
        assert!(matches!(
            AttestationPlanner::prepare_registration_plan(&trailing),
            Err(PlannerError::Cose(_))
        ));

        let plan = AttestationPlanner::prepare_registration_plan(&attestation).unwrap();

        assert_eq!(plan.signer, FIXTURE_SIGNER);
        assert_eq!(
            plan.signer,
            AttestationPlanner::signer_from_public_key(&FIXTURE_PUBLIC_KEY).unwrap()
        );
        // Fixture encodes `nonce: null`.
        assert_eq!(plan.nonce, None);
        assert_eq!(plan.root_cert_hash, PINNED_ROOT_CERT_HASH);
        assert_eq!(keccak256(&plan.root_cert), PINNED_ROOT_CERT_HASH);
        assert_eq!(plan.leaf_cert_hash, LEAF_HASH);
        assert_eq!(plan.signature.len(), 96);
        assert_eq!(keccak256(&plan.attestation_tbs), FIXTURE_TBS_KECCAK);
        // Raw protected TLV (not reserialized content-only).
        assert_eq!(&plan.attestation_tbs[12..17], &[0x44, 0xa1, 0x01, 0x38, 0x22]);
        assert_eq!(plan.attestation_tbs[17], 0x40);

        assert_eq!(plan.certs.len(), 4);
        assert_eq!(plan.certs[0].kind, CertKind::Ca);
        assert_eq!(plan.certs[0].cert_hash, REGIONAL_CA_HASH);
        assert_eq!(plan.certs[0].revocation_id, REGIONAL_CA_REVOCATION_ID);
        assert_eq!(plan.certs[0].parent_cert_hash, PINNED_ROOT_CERT_HASH);

        let leaf = plan.certs.last().unwrap();
        assert_eq!(leaf.kind, CertKind::Leaf);
        assert_eq!(leaf.cert_hash, LEAF_HASH);
        assert_eq!(leaf.revocation_id, LEAF_REVOCATION_ID);

        let mut trailing_der = leaf.cert.clone();
        trailing_der.push(0x00);
        assert!(CertManagerKeys::keys(&trailing_der).is_err());
    }

    #[test]
    fn require_v3_layout_rejects_trailing_bytes_in_version_wrapper() {
        // SEQUENCE { [0] EXPLICIT { INTEGER 2, leftover } } == a0050201020500 inside TBS.
        let trailing_wrapper = [0x30, 0x07, 0xa0, 0x05, 0x02, 0x01, 0x02, 0x05, 0x00];
        assert!(CertManagerKeys::require_v3_layout(&trailing_wrapper, X509Version::V3).is_err());

        let exact_wrapper = [0x30, 0x05, 0xa0, 0x03, 0x02, 0x01, 0x02];
        assert!(CertManagerKeys::require_v3_layout(&exact_wrapper, X509Version::V3).is_ok());
    }

    /// Deterministic nonce substituted into the Base fixture payload (`nonce: h'01020304'`).
    const FIXTURE_NONCE: [u8; 4] = [0x01, 0x02, 0x03, 0x04];

    fn base_fixture_with_nonce(nonce: &[u8]) -> Vec<u8> {
        let attestation = base_fixture_attestation();
        let cose = NitroCose::parse_sign1(&attestation).unwrap();
        let needle = [0x65, b'n', b'o', b'n', b'c', b'e', 0xf6];
        let pos = cose
            .payload
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("fixture encodes nonce: null");
        assert!(nonce.len() < 24);
        let mut payload = cose.payload[..pos + 6].to_vec();
        payload.push(0x40 | nonce.len() as u8);
        payload.extend_from_slice(nonce);
        payload.extend_from_slice(&cose.payload[pos + 7..]);

        let mut out = vec![0x84, 0x44, 0xa1, 0x01, 0x38, 0x22, 0xa0];
        let len = payload.len();
        if len < 24 {
            out.push(0x40 | len as u8);
        } else if len < 256 {
            out.push(0x58);
            out.push(len as u8);
        } else {
            out.push(0x59);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        out.extend_from_slice(&payload);
        out.push(0x58);
        out.push(96);
        out.extend_from_slice(&cose.signature);
        out
    }

    #[test]
    fn prepare_registration_plan_pins_non_null_nonce_bytes() {
        let attestation = base_fixture_with_nonce(&FIXTURE_NONCE);
        let plan = AttestationPlanner::prepare_registration_plan(&attestation).unwrap();
        assert_eq!(plan.nonce.as_deref(), Some(FIXTURE_NONCE.as_slice()));
        assert_eq!(plan.signer, FIXTURE_SIGNER);
    }
}
