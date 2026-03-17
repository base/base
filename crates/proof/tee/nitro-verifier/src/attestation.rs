//! `COSE_Sign1` parsing and attestation document extraction.
//!
//! Adapted from the Automata SDK's `cose.rs` and `doc.rs`, replacing the
//! unmaintained `serde_cbor` with `ciborium` for CBOR encoding/decoding.

use std::collections::BTreeMap;

use ciborium::Value as CborValue;
use serde::Deserialize;
use serde_bytes::{ByteArray, ByteBuf};

use crate::{Result, VerifierError};

/// A parsed `COSE_Sign1` envelope (RFC 8152 Section 4.2).
///
/// Contains the raw CBOR-encoded protected header, the attestation document
/// payload, and the signature. The envelope can be parsed from raw bytes
/// without verifying the signature — verification is handled separately by
/// the x509 chain validation in CHAIN-3559.
#[derive(Debug)]
pub struct CoseSign1 {
    /// CBOR-encoded protected header (contains the signature algorithm).
    pub protected: Vec<u8>,
    /// Raw attestation document payload (CBOR-encoded `AttestationDocument`).
    pub payload: Vec<u8>,
    /// COSE signature bytes.
    pub signature: Vec<u8>,
}

impl CoseSign1 {
    /// Parses a `COSE_Sign1` structure from raw CBOR bytes.
    ///
    /// Handles the optional CBOR tag 18 (`COSE_Sign1`) per RFC 8152.
    /// The input is expected to be a CBOR array of 4 elements:
    /// `[protected, unprotected, payload, signature]`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let value: CborValue = ciborium::de::from_reader(bytes)
            .map_err(|e| VerifierError::Cbor(format!("`COSE_Sign1`: {e}")))?;

        // Unwrap optional CBOR tag 18 (`COSE_Sign1`).
        let array = match value {
            CborValue::Tag(18, inner) => match *inner {
                CborValue::Array(a) => a,
                _ => return Err(VerifierError::CoseFormat("tagged value is not an array".into())),
            },
            CborValue::Array(a) => a,
            _ => return Err(VerifierError::CoseFormat("expected array or tag 18".into())),
        };

        if array.len() != 4 {
            return Err(VerifierError::CoseFormat(format!(
                "expected 4 elements, got {}",
                array.len()
            )));
        }

        let mut iter = array.into_iter();
        let protected = Self::extract_bytes(iter.next().unwrap(), "protected")?;
        // Skip unprotected header (element 1) — we don't use it.
        let _unprotected = iter.next().unwrap();
        let payload = Self::extract_bytes(iter.next().unwrap(), "payload")?;
        let signature = Self::extract_bytes(iter.next().unwrap(), "signature")?;

        Ok(Self { protected, payload, signature })
    }

    /// Constructs the CBOR-encoded `Sig_structure` for signature verification.
    ///
    /// Per RFC 8152 Section 4.4, the `Sig_structure` for `COSE_Sign1` is:
    /// `["Signature1", body_protected, external_aad, payload]`
    ///
    /// This is the to-be-signed (TBS) data that the signature covers.
    pub fn sig_structure(&self) -> Result<Vec<u8>> {
        let structure = CborValue::Array(vec![
            CborValue::Text("Signature1".to_string()),
            CborValue::Bytes(self.protected.clone()),
            CborValue::Bytes(vec![]), // external_aad — empty
            CborValue::Bytes(self.payload.clone()),
        ]);

        let mut buf = Vec::with_capacity(20 + self.protected.len() + self.payload.len());
        ciborium::ser::into_writer(&structure, &mut buf)
            .map_err(|e| VerifierError::Cbor(format!("sig_structure encode: {e}")))?;
        Ok(buf)
    }

    /// Extracts a byte string from a CBOR value.
    fn extract_bytes(value: CborValue, field: &str) -> Result<Vec<u8>> {
        match value {
            CborValue::Bytes(b) => Ok(b),
            _ => Err(VerifierError::CoseFormat(format!("{field} is not a byte string"))),
        }
    }
}

/// A parsed AWS Nitro Enclave attestation document.
///
/// Deserialized from the CBOR payload inside a `CoseSign1` envelope.
/// See: <https://docs.aws.amazon.com/enclaves/latest/user/verify-root.html>
#[derive(Debug, Deserialize)]
pub struct AttestationDocument {
    /// AWS Nitro Enclave module identifier.
    pub module_id: String,
    /// Attestation timestamp (Unix timestamp in milliseconds).
    pub timestamp: u64,
    /// Digest algorithm used (expected: `"SHA384"`).
    pub digest: String,
    /// Platform Configuration Registers — integrity measurements.
    pub pcrs: BTreeMap<u64, ByteArray<48>>,
    /// DER-encoded leaf certificate for the enclave.
    pub certificate: ByteBuf,
    /// DER-encoded CA bundle (root → intermediate chain).
    pub cabundle: Vec<ByteBuf>,
    /// Optional public key embedded in the attestation.
    pub public_key: Option<ByteBuf>,
    /// Optional user-defined data.
    pub user_data: Option<ByteBuf>,
    /// Optional nonce for replay protection.
    pub nonce: Option<ByteBuf>,
}

/// A parsed Nitro attestation report combining the COSE envelope and document.
#[derive(Debug)]
pub struct AttestationReport {
    /// The `COSE_Sign1` envelope (for signature verification).
    pub cose: CoseSign1,
    /// The parsed attestation document from the COSE payload.
    pub doc: AttestationDocument,
}

impl AttestationReport {
    /// Parses a raw Nitro attestation report (`COSE_Sign1` bytes).
    ///
    /// Extracts the `COSE_Sign1` envelope and deserializes the CBOR payload
    /// into an `AttestationDocument`.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let cose = CoseSign1::from_bytes(data)?;
        let doc: AttestationDocument =
            ciborium::de::from_reader(cose.payload.as_slice()).map_err(|e| {
                VerifierError::AttestationFormat(format!("attestation document decode: {e}"))
            })?;
        Ok(Self { cose, doc })
    }

    /// Extracts the DER-encoded certificates from the document (CA bundle + leaf).
    ///
    /// Returns the certificates in chain order: root → intermediates → leaf.
    /// Used by CHAIN-3559 for x509 chain verification.
    pub fn cert_chain_der(&self) -> Vec<&[u8]> {
        self.doc
            .cabundle
            .iter()
            .map(|c| c.as_ref())
            .chain(std::iter::once(self.doc.certificate.as_ref()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cose_from_bytes_rejects_empty_input() {
        assert!(CoseSign1::from_bytes(&[]).is_err());
    }

    #[test]
    fn cose_from_bytes_rejects_non_array() {
        // CBOR integer 42
        let cbor_int = vec![0x18, 0x2a];
        assert!(CoseSign1::from_bytes(&cbor_int).is_err());
    }

    #[test]
    fn cose_from_bytes_rejects_wrong_length_array() {
        // CBOR array of 3 byte strings
        let mut buf = Vec::new();
        ciborium::ser::into_writer(
            &CborValue::Array(vec![
                CborValue::Bytes(vec![]),
                CborValue::Bytes(vec![]),
                CborValue::Bytes(vec![]),
            ]),
            &mut buf,
        )
        .unwrap();
        assert!(CoseSign1::from_bytes(&buf).is_err());
    }

    #[test]
    fn cose_from_bytes_parses_valid_4_element_array() {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(
            &CborValue::Array(vec![
                CborValue::Bytes(vec![0x01]),       // protected
                CborValue::Map(vec![]),             // unprotected
                CborValue::Bytes(vec![0x02, 0x03]), // payload
                CborValue::Bytes(vec![0x04]),       // signature
            ]),
            &mut buf,
        )
        .unwrap();

        let cose = CoseSign1::from_bytes(&buf).unwrap();
        assert_eq!(cose.protected, [0x01]);
        assert_eq!(cose.payload, [0x02, 0x03]);
        assert_eq!(cose.signature, [0x04]);
    }

    #[test]
    fn cose_from_bytes_handles_tag_18() {
        let inner = CborValue::Array(vec![
            CborValue::Bytes(vec![0x01]),
            CborValue::Map(vec![]),
            CborValue::Bytes(vec![0x02]),
            CborValue::Bytes(vec![0x03]),
        ]);
        let tagged = CborValue::Tag(18, Box::new(inner));

        let mut buf = Vec::new();
        ciborium::ser::into_writer(&tagged, &mut buf).unwrap();

        let cose = CoseSign1::from_bytes(&buf).unwrap();
        assert_eq!(cose.protected, [0x01]);
    }

    #[test]
    fn cose_from_bytes_rejects_wrong_tag() {
        let inner = CborValue::Array(vec![
            CborValue::Bytes(vec![]),
            CborValue::Map(vec![]),
            CborValue::Bytes(vec![]),
            CborValue::Bytes(vec![]),
        ]);
        let tagged = CborValue::Tag(99, Box::new(inner));

        let mut buf = Vec::new();
        ciborium::ser::into_writer(&tagged, &mut buf).unwrap();

        // Tag 99 is neither Array nor Tag(18, ...) — rejected.
        assert!(CoseSign1::from_bytes(&buf).is_err());
    }

    #[test]
    fn sig_structure_produces_valid_cbor() {
        let cose = CoseSign1 {
            protected: vec![0xa1, 0x01, 0x38, 0x22], // {1: -35} (ES384)
            payload: vec![0x01, 0x02],
            signature: vec![0x03],
        };

        let tbs = cose.sig_structure().unwrap();
        // Should be a valid CBOR array
        let decoded: CborValue = ciborium::de::from_reader(tbs.as_slice()).unwrap();
        match decoded {
            CborValue::Array(elems) => {
                assert_eq!(elems.len(), 4);
                assert_eq!(elems[0], CborValue::Text("Signature1".to_string()));
            }
            _ => panic!("expected CBOR array"),
        }
    }
}
