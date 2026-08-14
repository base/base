//! Minimal CBOR walker matching `NitroValidator` (`decodeAttestationTbs` / `_parseAttestation`).
//!
//! Preserves raw protected/payload byte-string TLVs for attestation TBS construction.
//! The COSE envelope accepts only compact `0xD2` (optional) + compact `0x84`; any valid
//! unprotected map is skipped. Nested `pcrs` / `cabundle` may be definite or indefinite.
//!
//! Callers should use [`NitroCose`]; [`CborItem`] is a low-level detail of that parser.

use std::collections::BTreeSet;

use crate::error::{PlannerError, PlannerResult};

const MAX_CBOR_NESTING_DEPTH: usize = 64;
const MAX_PCRS: usize = 32;
const MAX_CABUNDLE_CERTS: usize = 32;
const MAX_CABUNDLE_CERT_BYTES: usize = 1024;
const P384_SIGNATURE_BYTES: usize = 96;

const CBOR_MAJOR_UINT: u8 = 0;
const CBOR_MAJOR_NEG_INT: u8 = 1;
const CBOR_MAJOR_BYTE_STRING: u8 = 2;
const CBOR_MAJOR_TEXT_STRING: u8 = 3;
const CBOR_MAJOR_ARRAY: u8 = 4;
const CBOR_MAJOR_MAP: u8 = 5;
const CBOR_MAJOR_TAG: u8 = 6;
const CBOR_MAJOR_SIMPLE: u8 = 7;
const CBOR_BREAK: u8 = 0xff;

const COSE_SIGN1_TAG_BYTE: u8 = 0xd2;
const COSE_SIGN1_ARRAY_BYTE: u8 = 0x84;
const CBOR_EMPTY_BYTE_STRING: u8 = 0x40;

/// Top-level Nitro fields that `NitroValidator` treats as single-assignment.
const KNOWN_ATTESTATION_KEYS: &[&str] = &[
    "module_id",
    "digest",
    "certificate",
    "public_key",
    "user_data",
    "nonce",
    "timestamp",
    "cabundle",
    "pcrs",
];

/// Encoded Nitro protected-header content selecting ES384 (`{1: -35}`) as a CBOR bstr TLV.
const NITRO_PROTECTED_HEADER_TLV: &[u8] = &[0x44, 0xa1, 0x01, 0x38, 0x22];

/// `Sig_structure` array header + `"Signature1"` context string.
const COSE_SIG_STRUCTURE_PREFIX: &[u8] =
    &[0x84, 0x6a, b'S', b'i', b'g', b'n', b'a', b't', b'u', b'r', b'e', b'1'];

/// Strictly parsed `COSE_Sign1` fields needed to build a registration plan.
#[derive(Debug)]
pub struct ParsedCoseSign1 {
    /// Attestation TBS built from raw protected/payload TLVs (no reserialization).
    pub attestation_tbs: Vec<u8>,
    /// Raw attestation document payload bytes (CBOR map content).
    pub payload: Vec<u8>,
    /// 96-byte P-384 signature (`r || s`).
    pub signature: Vec<u8>,
}

/// `NitroValidator`-aligned `COSE_Sign1` / payload structure parser.
///
/// This is the supported entry point for strict COSE/payload checks. Prefer
/// [`Self::parse_sign1`] and [`Self::validate_payload_structure`].
#[derive(Debug, Default)]
pub struct NitroCose;

/// Low-level CBOR item cursor used by [`NitroCose`].
///
/// Not a general-purpose CBOR API — prefer [`NitroCose`] for attestation parsing.
#[derive(Clone, Copy, Debug)]
pub struct CborItem {
    /// CBOR major type.
    pub major: u8,
    /// Decoded additional-information value (length, tag, integer, …).
    pub value: u64,
    /// Whether the item uses indefinite-length encoding.
    pub indefinite: bool,
    /// Start offset of the item's content (after the header).
    pub content_start: usize,
    /// End offset (exclusive) of the complete item.
    pub end: usize,
}

impl CborItem {
    /// Decodes the complete CBOR item at `start`.
    pub fn read(bytes: &[u8], start: usize) -> PlannerResult<Self> {
        Self::read_at(bytes, start, 0)
    }

    /// Decodes an item with `depth` enclosing containers already entered.
    pub fn read_at(bytes: &[u8], start: usize, depth: usize) -> PlannerResult<Self> {
        if depth > MAX_CBOR_NESTING_DEPTH {
            return Err(PlannerError::Cose(format!(
                "CBOR nesting exceeds maximum depth {MAX_CBOR_NESTING_DEPTH}"
            )));
        }
        if start >= bytes.len() {
            return Err(PlannerError::Cose("CBOR read out of bounds".into()));
        }

        let initial = bytes[start];
        let major = initial >> 5;
        let ai = initial & 0x1f;
        let (value, header_len, indefinite) = Self::read_additional_info(bytes, start + 1, ai)?;
        let content_start = start + header_len;
        let mut end = content_start;

        match major {
            CBOR_MAJOR_BYTE_STRING | CBOR_MAJOR_TEXT_STRING => {
                if indefinite {
                    return Err(PlannerError::Cose(
                        "unsupported indefinite CBOR byte/text string".into(),
                    ));
                }
                let len = usize::try_from(value).map_err(|_| {
                    PlannerError::Cose("CBOR item length exceeds platform address space".into())
                })?;
                let Some(content_end) = content_start.checked_add(len) else {
                    return Err(PlannerError::Cose("CBOR item length out of bounds".into()));
                };
                if content_end > bytes.len() {
                    return Err(PlannerError::Cose("CBOR item length out of bounds".into()));
                }
                end = content_end;
            }
            CBOR_MAJOR_ARRAY => {
                if indefinite {
                    loop {
                        if end >= bytes.len() {
                            return Err(PlannerError::Cose(
                                "indefinite CBOR array missing break".into(),
                            ));
                        }
                        if bytes[end] == CBOR_BREAK {
                            end += 1;
                            break;
                        }
                        let item = Self::read_at(bytes, end, depth + 1)?;
                        end = item.end;
                    }
                } else {
                    // Each array item is at least one byte.
                    let remaining = bytes.len().saturating_sub(end) as u64;
                    if value > remaining {
                        return Err(PlannerError::Cose(
                            "CBOR array claims more items than remaining bytes".into(),
                        ));
                    }
                    for _ in 0..value {
                        let item = Self::read_at(bytes, end, depth + 1)?;
                        end = item.end;
                    }
                }
            }
            CBOR_MAJOR_MAP => {
                if indefinite {
                    loop {
                        if end >= bytes.len() {
                            return Err(PlannerError::Cose(
                                "indefinite CBOR map missing break".into(),
                            ));
                        }
                        if bytes[end] == CBOR_BREAK {
                            end += 1;
                            break;
                        }
                        let key = Self::read_at(bytes, end, depth + 1)?;
                        let map_value = Self::read_at(bytes, key.end, depth + 1)?;
                        end = map_value.end;
                    }
                } else {
                    // Each map entry is at least a 1-byte key and 1-byte value.
                    let remaining = bytes.len().saturating_sub(end) as u64;
                    if value > remaining / 2 {
                        return Err(PlannerError::Cose(
                            "CBOR map claims more items than remaining bytes".into(),
                        ));
                    }
                    for _ in 0..value {
                        let key = Self::read_at(bytes, end, depth + 1)?;
                        let map_value = Self::read_at(bytes, key.end, depth + 1)?;
                        end = map_value.end;
                    }
                }
            }
            CBOR_MAJOR_UINT | CBOR_MAJOR_NEG_INT => {
                if indefinite {
                    return Err(PlannerError::Cose(format!(
                        "unsupported indefinite CBOR major type {major}"
                    )));
                }
            }
            CBOR_MAJOR_TAG => {
                if indefinite {
                    return Err(PlannerError::Cose("unsupported indefinite CBOR tag".into()));
                }
                let tagged = Self::read_at(bytes, content_start, depth + 1)?;
                end = tagged.end;
            }
            CBOR_MAJOR_SIMPLE => {
                if indefinite {
                    return Err(PlannerError::Cose("unexpected CBOR break marker".into()));
                }
            }
            _ => {
                return Err(PlannerError::Cose(format!("unsupported CBOR major type {major}")));
            }
        }

        if end > bytes.len() {
            return Err(PlannerError::Cose("CBOR item length out of bounds".into()));
        }
        Ok(Self { major, value, indefinite, content_start, end })
    }

    /// Decodes the additional-information portion of a CBOR header.
    ///
    /// Returns `(value, header_length_including_initial_byte, indefinite)`.
    pub fn read_additional_info(
        bytes: &[u8],
        offset: usize,
        ai: u8,
    ) -> PlannerResult<(u64, usize, bool)> {
        match ai {
            0..=23 => Ok((u64::from(ai), 1, false)),
            24 => {
                if offset >= bytes.len() {
                    return Err(PlannerError::Cose("CBOR uint8 out of bounds".into()));
                }
                Ok((u64::from(bytes[offset]), 2, false))
            }
            25 => {
                if offset + 2 > bytes.len() {
                    return Err(PlannerError::Cose("CBOR uint16 out of bounds".into()));
                }
                Ok((u64::from(u16::from_be_bytes([bytes[offset], bytes[offset + 1]])), 3, false))
            }
            26 => {
                if offset + 4 > bytes.len() {
                    return Err(PlannerError::Cose("CBOR uint32 out of bounds".into()));
                }
                Ok((
                    u64::from(u32::from_be_bytes([
                        bytes[offset],
                        bytes[offset + 1],
                        bytes[offset + 2],
                        bytes[offset + 3],
                    ])),
                    5,
                    false,
                ))
            }
            27 => {
                if offset + 8 > bytes.len() {
                    return Err(PlannerError::Cose("CBOR uint64 out of bounds".into()));
                }
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&bytes[offset..offset + 8]);
                Ok((u64::from_be_bytes(buf), 9, false))
            }
            31 => Ok((0, 1, true)),
            _ => Err(PlannerError::Cose(format!("unsupported CBOR additional information {ai}"))),
        }
    }

    /// Ensures this item has the expected CBOR major type.
    pub fn require_major(self, major: u8, label: &str) -> PlannerResult<Self> {
        if self.major != major {
            return Err(PlannerError::Cose(format!(
                "{label} has unexpected CBOR major type {}",
                self.major
            )));
        }
        Ok(self)
    }
}

impl NitroCose {
    /// Parses a `COSE_Sign1` attestation with `NitroValidator.decodeAttestationTbs` envelope checks.
    ///
    /// Solidity only accepts compact tag `0xD2` and compact array `0x84`, then skips any valid
    /// unprotected map before the payload. Long-form outer array/tag encodings are rejected.
    pub fn parse_sign1(attestation: &[u8]) -> PlannerResult<ParsedCoseSign1> {
        if attestation.is_empty() {
            return Err(PlannerError::Cose("empty attestation".into()));
        }

        let mut offset = 0usize;
        if attestation[0] == COSE_SIGN1_TAG_BYTE {
            offset = 1;
        }
        if offset >= attestation.len() || attestation[offset] != COSE_SIGN1_ARRAY_BYTE {
            return Err(PlannerError::Cose(
                "COSE_Sign1 must use compact array header 0x84 (optional compact tag 0xD2)".into(),
            ));
        }
        offset += 1;

        let protected = CborItem::read(attestation, offset)?
            .require_major(CBOR_MAJOR_BYTE_STRING, "protected header")?;
        let unprotected = CborItem::read(attestation, protected.end)?
            .require_major(CBOR_MAJOR_MAP, "unprotected header")?;
        let payload = CborItem::read(attestation, unprotected.end)?
            .require_major(CBOR_MAJOR_BYTE_STRING, "payload")?;
        let signature = CborItem::read(attestation, payload.end)?
            .require_major(CBOR_MAJOR_BYTE_STRING, "signature")?;

        let sig_len = signature.end - signature.content_start;
        if sig_len != P384_SIGNATURE_BYTES {
            return Err(PlannerError::Cose(format!(
                "COSE_Sign1 signature must be {P384_SIGNATURE_BYTES} bytes, got {sig_len}"
            )));
        }
        if signature.end != attestation.len() {
            return Err(PlannerError::Cose("trailing data after COSE_Sign1 message".into()));
        }

        let raw_protected = &attestation[offset..protected.end];
        if raw_protected != NITRO_PROTECTED_HEADER_TLV {
            return Err(PlannerError::Cose("COSE_Sign1 protected header must select ES384".into()));
        }
        let raw_payload_tlv = &attestation[unprotected.end..payload.end];

        let mut attestation_tbs = Vec::with_capacity(
            COSE_SIG_STRUCTURE_PREFIX.len() + 1 + raw_protected.len() + raw_payload_tlv.len(),
        );
        attestation_tbs.extend_from_slice(COSE_SIG_STRUCTURE_PREFIX);
        attestation_tbs.extend_from_slice(raw_protected);
        attestation_tbs.push(CBOR_EMPTY_BYTE_STRING);
        attestation_tbs.extend_from_slice(raw_payload_tlv);

        Ok(ParsedCoseSign1 {
            attestation_tbs,
            payload: attestation[payload.content_start..payload.end].to_vec(),
            signature: attestation[signature.content_start..signature.end].to_vec(),
        })
    }

    /// Validates payload CBOR structure: no trailing bytes, PCR duplicate/index rules, cabundle
    /// limits.
    pub fn validate_payload_structure(payload: &[u8]) -> PlannerResult<()> {
        let root =
            CborItem::read(payload, 0)?.require_major(CBOR_MAJOR_MAP, "attestation payload")?;
        if root.end != payload.len() {
            return Err(PlannerError::Cose("trailing data after attestation payload".into()));
        }

        let mut offset = root.content_start;
        let mut item_count = 0u64;
        let mut seen_keys = BTreeSet::new();
        loop {
            if !root.indefinite && item_count == root.value {
                break;
            }
            if root.indefinite {
                if offset >= payload.len() {
                    return Err(PlannerError::Cose(
                        "indefinite attestation payload map missing break".into(),
                    ));
                }
                if payload[offset] == CBOR_BREAK {
                    break;
                }
            }

            let key = CborItem::read(payload, offset)?
                .require_major(CBOR_MAJOR_TEXT_STRING, "attestation payload key")?;
            let value = CborItem::read(payload, key.end)?;
            let value_end = value.end;
            let key_text = std::str::from_utf8(&payload[key.content_start..key.end])
                .map_err(|_| PlannerError::Cose("attestation payload key is not UTF-8".into()))?;
            if Self::is_known_attestation_key(key_text) && !seen_keys.insert(key_text) {
                return Err(PlannerError::Cose(format!(
                    "duplicate attestation payload key: {key_text}"
                )));
            }

            match key_text {
                "pcrs" => Self::validate_pcrs(payload, value)?,
                "cabundle" => Self::validate_cabundle(payload, value)?,
                "certificate" => {
                    let certificate = value.require_major(CBOR_MAJOR_BYTE_STRING, "certificate")?;
                    let len = certificate.end - certificate.content_start;
                    if len == 0 || len > MAX_CABUNDLE_CERT_BYTES {
                        return Err(PlannerError::Cose(format!(
                            "certificate must be between 1 and {MAX_CABUNDLE_CERT_BYTES} bytes, got {len}"
                        )));
                    }
                }
                _ => {}
            }

            offset = value_end;
            item_count += 1;
        }

        if root.indefinite {
            if offset >= payload.len() || payload[offset] != CBOR_BREAK {
                return Err(PlannerError::Cose(
                    "indefinite attestation payload map missing break".into(),
                ));
            }
        } else if offset != root.end {
            return Err(PlannerError::Cose("attestation payload map length mismatch".into()));
        }

        Ok(())
    }

    /// Validates the `pcrs` map: unique keys below 32, definite or indefinite encoding.
    ///
    /// Matches `NitroValidator._parsePcrs` (32-slot bank, sparse indices allowed). Contiguous
    /// `0..count-1` is an older Agora/contract invariant and is not required.
    fn validate_pcrs(payload: &[u8], pcrs: CborItem) -> PlannerResult<()> {
        let pcrs = pcrs.require_major(CBOR_MAJOR_MAP, "pcrs")?;
        if !pcrs.indefinite && pcrs.value > MAX_PCRS as u64 {
            return Err(PlannerError::Cose(format!(
                "PCR count {} out of range (must be at most {MAX_PCRS})",
                pcrs.value
            )));
        }

        let mut seen = BTreeSet::new();
        let mut offset = pcrs.content_start;
        let mut count = 0u64;
        loop {
            if Self::container_done(payload, pcrs, offset, count, "pcrs")? {
                break;
            }
            if count >= MAX_PCRS as u64 {
                return Err(PlannerError::Cose(format!(
                    "attestation pcrs has more than {MAX_PCRS} entries"
                )));
            }
            let key = CborItem::read(payload, offset)?.require_major(CBOR_MAJOR_UINT, "pcr key")?;
            if key.value >= MAX_PCRS as u64 {
                return Err(PlannerError::Cose(format!(
                    "PCR index {} out of range (must be 0-{})",
                    key.value,
                    MAX_PCRS - 1
                )));
            }
            if !seen.insert(key.value) {
                return Err(PlannerError::Cose(format!("duplicate PCR index {}", key.value)));
            }
            let value = CborItem::read(payload, key.end)?
                .require_major(CBOR_MAJOR_BYTE_STRING, "pcr value")?;
            offset = value.end;
            count += 1;
        }
        Ok(())
    }

    /// Validates cabundle length and per-certificate size limits (definite or indefinite).
    fn validate_cabundle(payload: &[u8], cabundle: CborItem) -> PlannerResult<()> {
        let cabundle = cabundle.require_major(CBOR_MAJOR_ARRAY, "cabundle")?;
        if !cabundle.indefinite && cabundle.value > MAX_CABUNDLE_CERTS as u64 {
            return Err(PlannerError::Cose(format!(
                "cabundle has {} certificates, must be at most {MAX_CABUNDLE_CERTS}",
                cabundle.value
            )));
        }

        let mut offset = cabundle.content_start;
        let mut count = 0u64;
        loop {
            if Self::container_done(payload, cabundle, offset, count, "cabundle")? {
                break;
            }
            if count >= MAX_CABUNDLE_CERTS as u64 {
                return Err(PlannerError::Cose(format!(
                    "cabundle has more than {MAX_CABUNDLE_CERTS} certificates"
                )));
            }
            let item = CborItem::read(payload, offset)?
                .require_major(CBOR_MAJOR_BYTE_STRING, "cabundle certificate")?;
            let len = item.end - item.content_start;
            if len == 0 || len > MAX_CABUNDLE_CERT_BYTES {
                return Err(PlannerError::Cose(format!(
                    "cabundle[{count}] must be between 1 and {MAX_CABUNDLE_CERT_BYTES} bytes, got {len}"
                )));
            }
            offset = item.end;
            count += 1;
        }
        if count == 0 {
            return Err(PlannerError::Cose(
                "cabundle must contain at least one certificate".into(),
            ));
        }
        Ok(())
    }

    fn is_known_attestation_key(key: &str) -> bool {
        KNOWN_ATTESTATION_KEYS.contains(&key)
    }

    fn container_done(
        bytes: &[u8],
        container: CborItem,
        offset: usize,
        item_count: u64,
        label: &str,
    ) -> PlannerResult<bool> {
        if !container.indefinite {
            return Ok(item_count == container.value);
        }
        if offset >= bytes.len() {
            return Err(PlannerError::Cose(format!("indefinite {label} missing break")));
        }
        Ok(bytes[offset] == CBOR_BREAK)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal definite `COSE_Sign1`: ES384 protected, empty unprotected, 1-byte payload, 96-byte sig.
    fn minimal_cose(payload: &[u8], signature: &[u8; 96]) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(0x84); // array(4)
        out.extend_from_slice(NITRO_PROTECTED_HEADER_TLV);
        out.push(0xa0); // empty unprotected map
        // payload bstr
        assert!(payload.len() < 24);
        out.push(0x40 | payload.len() as u8);
        out.extend_from_slice(payload);
        out.push(0x58); // bstr, 1-byte length
        out.push(96);
        out.extend_from_slice(signature);
        out
    }

    fn fixture_attestation() -> Vec<u8> {
        hex::decode(include_str!("testdata/nitro_attestation.hex").trim()).unwrap()
    }

    #[test]
    fn parse_sign1_accepts_fixture_and_rejects_trailing() {
        let mut attestation = fixture_attestation();
        assert!(NitroCose::parse_sign1(&attestation).is_ok());
        attestation.push(0x00);
        assert!(matches!(NitroCose::parse_sign1(&attestation), Err(PlannerError::Cose(_))));
    }

    #[test]
    fn parse_sign1_matches_nitro_validator_envelope() {
        let sig = [0u8; 96];
        let good = minimal_cose(&[0xa0], &sig);
        assert!(NitroCose::parse_sign1(&good).is_ok());

        // Compact tag 0xD2 is accepted; long-form tag(18) is not.
        let mut tagged = vec![0xd2];
        tagged.extend_from_slice(&good);
        assert!(NitroCose::parse_sign1(&tagged).is_ok());
        let mut long_tag = vec![0xd8, 0x12];
        long_tag.extend_from_slice(&good);
        assert!(NitroCose::parse_sign1(&long_tag).is_err());

        // Long-form array(4) header is rejected (Solidity only reads compact 0x84).
        let mut long_array = vec![0x98, 0x04];
        long_array.extend_from_slice(&good[1..]);
        assert!(NitroCose::parse_sign1(&long_array).is_err());

        // Non-empty unprotected map is skipped, matching `skipValue`.
        let mut nonempty = Vec::new();
        nonempty.push(0x84);
        nonempty.extend_from_slice(NITRO_PROTECTED_HEADER_TLV);
        nonempty.extend_from_slice(&[0xa1, 0x01, 0x01]); // map(1): 1 => 1
        nonempty.extend_from_slice(&[0x41, 0xa0]);
        nonempty.push(0x58);
        nonempty.push(96);
        nonempty.extend_from_slice(&sig);
        assert!(NitroCose::parse_sign1(&nonempty).is_ok());

        // Indefinite unprotected map is also skipped.
        let mut indef_unprotected = Vec::new();
        indef_unprotected.push(0x84);
        indef_unprotected.extend_from_slice(NITRO_PROTECTED_HEADER_TLV);
        indef_unprotected.extend_from_slice(&[0xbf, 0xff]);
        indef_unprotected.extend_from_slice(&[0x41, 0xa0]);
        indef_unprotected.push(0x58);
        indef_unprotected.push(96);
        indef_unprotected.extend_from_slice(&sig);
        assert!(NitroCose::parse_sign1(&indef_unprotected).is_ok());

        // Wrong array length.
        let mut wrong_len = Vec::new();
        wrong_len.push(0x83);
        wrong_len.extend_from_slice(NITRO_PROTECTED_HEADER_TLV);
        wrong_len.push(0xa0);
        wrong_len.extend_from_slice(&[0x41, 0xa0]);
        assert!(NitroCose::parse_sign1(&wrong_len).is_err());

        // Non-ES384 protected header content.
        let mut bad_protected = Vec::new();
        bad_protected.push(0x84);
        bad_protected.extend_from_slice(&[0x41, 0x00]);
        bad_protected.push(0xa0);
        bad_protected.extend_from_slice(&[0x41, 0xa0]);
        bad_protected.push(0x58);
        bad_protected.push(96);
        bad_protected.extend_from_slice(&sig);
        assert!(NitroCose::parse_sign1(&bad_protected).is_err());

        // Unexpected CBOR tag.
        let mut tagged_wrong = vec![0xd8, 0x63];
        tagged_wrong.extend_from_slice(&good);
        assert!(NitroCose::parse_sign1(&tagged_wrong).is_err());

        // Indefinite-length array is rejected.
        assert!(NitroCose::parse_sign1(&[0x9f, 0xff]).is_err());

        // Wrong signature length (1 byte instead of 96).
        let mut short_sig = Vec::new();
        short_sig.push(0x84);
        short_sig.extend_from_slice(NITRO_PROTECTED_HEADER_TLV);
        short_sig.push(0xa0);
        short_sig.extend_from_slice(&[0x41, 0xa0]);
        short_sig.extend_from_slice(&[0x41, 0x00]);
        assert!(NitroCose::parse_sign1(&short_sig).is_err());
    }

    #[test]
    fn cbor_item_rejects_truncated_header_oversize_and_deep_nesting() {
        assert!(CborItem::read(&[0x18], 0).is_err()); // uint8 AI missing byte
        assert!(CborItem::read(&[0x45, 0x00, 0x00], 0).is_err()); // bstr len 5, only 2 bytes

        // Nest array(1) deeper than MAX_CBOR_NESTING_DEPTH.
        let mut nested = vec![0x81u8; MAX_CBOR_NESTING_DEPTH + 2];
        nested.push(0x00);
        assert!(CborItem::read(&nested, 0).is_err());
    }

    #[test]
    fn validate_payload_structure_rejects_pcr_and_cabundle_faults() {
        // Trailing byte after empty map.
        assert!(NitroCose::validate_payload_structure(&[0xa0, 0x00]).is_err());

        // pcrs with duplicate key 0.
        // {"pcrs": {0: h'00', 0: h'00'}}
        let dup_pcrs =
            [0xa1, 0x64, b'p', b'c', b'r', b's', 0xa2, 0x00, 0x41, 0x00, 0x00, 0x41, 0x00];
        assert!(NitroCose::validate_payload_structure(&dup_pcrs).is_err());

        // Duplicate top-level key: {"pcrs": {0: h'00'}, "pcrs": {0: h'00'}}.
        let dup_top = [
            0xa2, 0x64, b'p', b'c', b'r', b's', 0xa1, 0x00, 0x41, 0x00, 0x64, b'p', b'c', b'r',
            b's', 0xa1, 0x00, 0x41, 0x00,
        ];
        assert!(NitroCose::validate_payload_structure(&dup_top).is_err());

        // Repeated unknown keys stay forward-compatible.
        // {"x": null, "x": null}
        let dup_unknown = [0xa2, 0x61, b'x', 0xf6, 0x61, b'x', 0xf6];
        assert!(NitroCose::validate_payload_structure(&dup_unknown).is_ok());

        // Sparse PCR keys are valid (`{1: h'00'}`).
        let sparse = [0xa1, 0x64, b'p', b'c', b'r', b's', 0xa1, 0x01, 0x41, 0x00];
        assert!(NitroCose::validate_payload_structure(&sparse).is_ok());

        // Indefinite pcrs map with sparse key 8.
        // {"pcrs": {_ 0: h'00', 8: h'00' }}
        let indef_pcrs =
            [0xa1, 0x64, b'p', b'c', b'r', b's', 0xbf, 0x00, 0x41, 0x00, 0x08, 0x41, 0x00, 0xff];
        assert!(NitroCose::validate_payload_structure(&indef_pcrs).is_ok());

        // cabundle with empty cert bytes.
        // {"cabundle": [h'']}
        let empty_cert = [0xa1, 0x68, b'c', b'a', b'b', b'u', b'n', b'd', b'l', b'e', 0x81, 0x40];
        assert!(NitroCose::validate_payload_structure(&empty_cert).is_err());

        // cabundle with zero certificates.
        // {"cabundle": []}
        let empty_bundle = [0xa1, 0x68, b'c', b'a', b'b', b'u', b'n', b'd', b'l', b'e', 0x80];
        assert!(NitroCose::validate_payload_structure(&empty_bundle).is_err());

        // Indefinite cabundle with one cert.
        // {"cabundle": [_ h'30' ]}
        let indef_bundle =
            [0xa1, 0x68, b'c', b'a', b'b', b'u', b'n', b'd', b'l', b'e', 0x9f, 0x41, 0x30, 0xff];
        assert!(NitroCose::validate_payload_structure(&indef_bundle).is_ok());
    }

    #[test]
    fn cbor_item_rejects_impossible_container_counts() {
        // Definite array claiming more items than remaining bytes.
        assert!(CborItem::read(&[0x82], 0).is_err()); // array(2), 0 bytes left
        // Definite map claiming more entries than remaining bytes allow.
        assert!(CborItem::read(&[0xa2, 0x00], 0).is_err()); // map(2), only 1 byte left
    }

    #[test]
    fn validate_payload_structure_accepts_fixture_payload() {
        let cose = NitroCose::parse_sign1(&fixture_attestation()).unwrap();
        assert!(NitroCose::validate_payload_structure(&cose.payload).is_ok());
    }
}
