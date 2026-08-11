//! Nitro attestation registration-plan construction.
//!
//! Reuses [`AttestationReport`] for COSE / document parsing and adds the
//! `CertManager`-oriented plan fields required by hinted registration.

use alloy_primitives::{Address, B256, keccak256};
use base_proof_tee_nitro_verifier::AttestationReport;
use k256::ecdsa::VerifyingKey;
use x509_parser::prelude::FromDer;

use crate::{
    error::{PlannerError, PlannerResult},
    types::{CertKind, CertPlan, RegistrationPlan},
};

/// Encoded Nitro protected-header content selecting ES384 (`{1: -35}`).
const NITRO_PROTECTED_HEADER: &[u8] = &[0xa1, 0x01, 0x38, 0x22];
const P384_SIGNATURE_BYTES: usize = 96;
const UNCOMPRESSED_SECP256K1_LEN: usize = 65;

/// Full-DER hash of the pinned AWS Nitro root certificate.
pub const PINNED_ROOT_CERT_HASH: B256 = B256::new(hex_literal_root());

const fn hex_literal_root() -> [u8; 32] {
    // 0x311d96fcd5c5e0ccf72ef548e2ea7d4c0cd53ad7c4cc49e67471aed41d61f185
    [
        0x31, 0x1d, 0x96, 0xfc, 0xd5, 0xc5, 0xe0, 0xcc, 0xf7, 0x2e, 0xf5, 0x48, 0xe2, 0xea, 0x7d,
        0x4c, 0x0c, 0xd5, 0x3a, 0xd7, 0xc4, 0xcc, 0x49, 0xe6, 0x74, 0x71, 0xae, 0xd4, 0x1d, 0x61,
        0xf1, 0x85,
    ]
}

/// Parses AWS Nitro `COSE_Sign1` attestations into certificate registration plans.
#[derive(Debug, Default)]
pub struct AttestationPlanner;

impl AttestationPlanner {
    /// Parses a `COSE_Sign1` Nitro attestation and builds a registration plan.
    ///
    /// Does not generate P-384 inverse hints or submit transactions. The signer is
    /// derived from attestation `public_key` (Base semantics), never from `user_data`.
    pub fn prepare_registration_plan(attestation: &[u8]) -> PlannerResult<RegistrationPlan> {
        Self::from_report(&AttestationReport::parse(attestation)?)
    }

    /// Builds a registration plan from an already-parsed attestation report.
    pub fn from_report(report: &AttestationReport) -> PlannerResult<RegistrationPlan> {
        Self::validate_report(report)?;

        let public_key = report.doc.public_key.as_ref().ok_or_else(|| {
            PlannerError::Attestation("attestation payload missing public_key".into())
        })?;
        let signer = Self::signer_from_public_key(public_key)?;

        let pcr0 = report
            .doc
            .pcrs
            .get(&0)
            .ok_or_else(|| PlannerError::Attestation("attestation payload missing PCR0".into()))?
            .as_slice()
            .to_vec();

        if report.doc.cabundle.len() < 2 {
            return Err(PlannerError::Attestation(
                "attestation cabundle must include root plus at least one non-root CA".into(),
            ));
        }

        let root_cert = report.doc.cabundle[0].as_ref();
        let root_hash = keccak256(root_cert);
        if root_hash != PINNED_ROOT_CERT_HASH {
            return Err(PlannerError::Attestation(format!(
                "attestation root certificate hash {root_hash} is not trusted"
            )));
        }

        // Capacity = non-root CAs (`cabundle.len() - 1`) + leaf.
        let mut parent_hash = root_hash;
        let mut certs = Vec::with_capacity(report.doc.cabundle.len());
        for (i, cert) in report.doc.cabundle.iter().enumerate().skip(1) {
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

        let leaf = report.doc.certificate.as_ref();
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
            timestamp: report.doc.timestamp,
            nonce: report.doc.nonce.as_ref().map(|n| n.to_vec()),
            root_cert_hash: root_hash,
            leaf_cert_hash: leaf_hash,
            attestation_tbs: report.cose.sig_structure()?,
            signature: report.cose.signature.clone(),
            certs,
        })
    }

    fn validate_report(report: &AttestationReport) -> PlannerResult<()> {
        if report.cose.protected != NITRO_PROTECTED_HEADER {
            return Err(PlannerError::Attestation(
                "COSE_Sign1 protected header must select ES384".into(),
            ));
        }
        if report.cose.signature.len() != P384_SIGNATURE_BYTES {
            return Err(PlannerError::Attestation(format!(
                "COSE_Sign1 signature must be {P384_SIGNATURE_BYTES} bytes, got {}",
                report.cose.signature.len()
            )));
        }
        if report.doc.module_id.is_empty() {
            return Err(PlannerError::Attestation("attestation payload missing module_id".into()));
        }
        if report.doc.digest != "SHA384" {
            return Err(PlannerError::Attestation("attestation digest must be SHA384".into()));
        }
        if report.doc.timestamp == 0 {
            return Err(PlannerError::Attestation("attestation timestamp must be positive".into()));
        }
        Ok(())
    }

    fn ca_label(index: usize) -> String {
        match index {
            1 => "regional CA".into(),
            2 => "zonal CA".into(),
            3 => "issuer / instance CA".into(),
            _ => format!("non-root CA {index}"),
        }
    }

    fn signer_from_public_key(public_key: &[u8]) -> PlannerResult<Address> {
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
    pub fn keys(cert: &[u8]) -> PlannerResult<(B256, B256)> {
        let full_hash = keccak256(cert);
        let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(cert)
            .map_err(|e| PlannerError::Certificate(Box::new(e)))?;

        let cache_key = if full_hash == PINNED_ROOT_CERT_HASH {
            full_hash
        } else {
            keccak256(parsed.tbs_certificate.as_ref())
        };

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

    /// Strips the DER tag and length from a TLV, returning content octets.
    fn der_content_octets(tlv: &[u8]) -> PlannerResult<&[u8]> {
        if tlv.len() < 2 {
            return Err(Self::cert_error("DER TLV too short"));
        }
        let first = tlv[1];
        let (header_len, content_len) = if first & 0x80 == 0 {
            (2usize, first as usize)
        } else {
            let nbytes = (first & 0x7f) as usize;
            if nbytes == 0 || nbytes > 4 || tlv.len() < 2 + nbytes {
                return Err(Self::cert_error("invalid DER length"));
            }
            let mut len = 0usize;
            for b in &tlv[2..2 + nbytes] {
                len = (len << 8) | usize::from(*b);
            }
            (2 + nbytes, len)
        };
        let end = header_len
            .checked_add(content_len)
            .ok_or_else(|| Self::cert_error("DER length overflow"))?;
        if end != tlv.len() {
            return Err(Self::cert_error("DER TLV length mismatch"));
        }
        Ok(&tlv[header_len..end])
    }

    fn cert_error(message: &'static str) -> PlannerError {
        PlannerError::Certificate(Box::new(std::io::Error::other(message)))
    }
}

#[cfg(test)]
mod tests {
    use base_proof_tee_nitro_verifier::AttestationReport;
    use k256::ecdsa::SigningKey;

    use super::*;

    fn fixture_attestation() -> Vec<u8> {
        hex::decode(include_str!("testdata/nitro_attestation.hex").trim()).unwrap()
    }

    fn leaf_cert_der() -> Vec<u8> {
        AttestationReport::parse(&fixture_attestation()).unwrap().doc.certificate.to_vec()
    }

    #[test]
    fn cert_manager_keys_use_tbs_cache_key_and_issuer_serial_id() {
        let cert = leaf_cert_der();
        let (cache_key, revocation_id) = CertManagerKeys::keys(&cert).unwrap();

        // Non-root cache keys are TBS hashes, not full-DER hashes.
        assert_ne!(cache_key, keccak256(&cert));
        assert_ne!(revocation_id, cache_key);
        assert_eq!(cache_key, CertManagerKeys::cache_key(&cert).unwrap());
        assert_eq!(revocation_id, CertManagerKeys::revocation_id(&cert).unwrap());
    }

    #[test]
    fn signer_from_public_key_rejects_bad_keys() {
        assert!(AttestationPlanner::signer_from_public_key(&[]).is_err());
        assert!(AttestationPlanner::signer_from_public_key(&[0x04; 64]).is_err());
        let mut bad = SigningKey::from_bytes(&[7u8; 32].into())
            .unwrap()
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec();
        bad[0] = 0x02;
        assert!(AttestationPlanner::signer_from_public_key(&bad).is_err());
    }

    #[test]
    fn prepare_registration_plan_from_fixture() {
        // Agora's stable nitro fixture omits `public_key`; inject a valid secp256k1 key so the
        // full planner path (root pin, CA walk, leaf, TBS, signer) is exercised.
        let mut report = AttestationReport::parse(&fixture_attestation()).unwrap();
        let public_key = SigningKey::from_bytes(&[7u8; 32].into())
            .unwrap()
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec();
        report.doc.public_key = Some(serde_bytes::ByteBuf::from(public_key.clone()));

        let plan = AttestationPlanner::from_report(&report).unwrap();

        assert_eq!(plan.root_cert_hash, PINNED_ROOT_CERT_HASH);
        assert_eq!(plan.signature.len(), P384_SIGNATURE_BYTES);
        assert_eq!(plan.timestamp, report.doc.timestamp);
        assert_eq!(plan.pcr0, report.doc.pcrs.get(&0).unwrap().as_slice());
        assert_eq!(plan.attestation_tbs, report.cose.sig_structure().unwrap());
        assert_eq!(plan.signer, AttestationPlanner::signer_from_public_key(&public_key).unwrap());

        // Plan contains every non-root CA plus the leaf.
        assert_eq!(plan.certs.len(), report.doc.cabundle.len());
        assert!(plan.certs.iter().take(plan.certs.len() - 1).all(|c| c.kind == CertKind::Ca));
        assert_eq!(plan.certs.last().unwrap().kind, CertKind::Leaf);
        assert_eq!(plan.leaf_cert_hash, plan.certs.last().unwrap().cert_hash);

        // Parent links: first CA parent is pinned root; each next parent is prior cache key.
        assert_eq!(plan.certs[0].parent_cert_hash, PINNED_ROOT_CERT_HASH);
        for window in plan.certs.windows(2) {
            assert_eq!(window[1].parent_cert_hash, window[0].cert_hash);
        }
    }
}
