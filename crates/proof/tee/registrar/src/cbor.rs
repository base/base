//! Minimal CBOR walker for Solidity/`NitroValidator`-aligned `COSE_Sign1` parsing.
//!
//! Preserves raw protected/payload byte-string TLVs for attestation TBS construction
//! and rejects trailing bytes / non-map unprotected headers that `ciborium` accepts.

use std::collections::BTreeSet;

use crate::error::{PlannerError, PlannerResult};

const MAX_CBOR_NESTING_DEPTH: usize = 16;
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

const COSE_SIGN1_TAG: u64 = 18;
const CBOR_EMPTY_BYTE_STRING: u8 = 0x40;

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
#[derive(Debug, Default)]
pub struct NitroCose;

/// One decoded CBOR item with byte-range metadata in the source buffer.
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
                let Some(content_end) = content_start.checked_add(value as usize) else {
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
    /// Parses a `COSE_Sign1` attestation with `NitroValidator`-equivalent envelope checks.
    pub fn parse_sign1(attestation: &[u8]) -> PlannerResult<ParsedCoseSign1> {
        if attestation.is_empty() {
            return Err(PlannerError::Cose("empty attestation".into()));
        }

        let root = CborItem::read(attestation, 0)?;
        if root.end != attestation.len() {
            return Err(PlannerError::Cose("trailing data after COSE_Sign1 message".into()));
        }

        let mut array_start = 0;
        if root.major == CBOR_MAJOR_TAG {
            if root.value != COSE_SIGN1_TAG {
                return Err(PlannerError::Cose(format!(
                    "COSE_Sign1 has unexpected CBOR tag {}",
                    root.value
                )));
            }
            array_start = root.content_start;
        }
        let array = CborItem::read(attestation, array_start)?
            .require_major(CBOR_MAJOR_ARRAY, "COSE_Sign1")?;
        if array.indefinite || array.value != 4 {
            return Err(PlannerError::Cose(format!(
                "COSE_Sign1 must be a definite-length array of 4 items, got {}",
                array.value
            )));
        }

        let protected = CborItem::read(attestation, array.content_start)?
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
        if signature.end != array.end {
            return Err(PlannerError::Cose("COSE_Sign1 contains trailing array data".into()));
        }

        let raw_protected = &attestation[array.content_start..protected.end];
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

    /// Validates the `pcrs` map for duplicates, index bounds, and contiguous keys.
    pub fn validate_pcrs(payload: &[u8], pcrs: CborItem) -> PlannerResult<()> {
        let pcrs = pcrs.require_major(CBOR_MAJOR_MAP, "pcrs")?;
        if pcrs.indefinite {
            return Err(PlannerError::Cose("pcrs map must be definite-length".into()));
        }
        if pcrs.value == 0 || pcrs.value > MAX_PCRS as u64 {
            return Err(PlannerError::Cose(format!(
                "PCR count {} out of range (must be 1-{MAX_PCRS})",
                pcrs.value
            )));
        }

        let mut seen = BTreeSet::new();
        let mut offset = pcrs.content_start;
        for _ in 0..pcrs.value {
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
        }

        // Mirror NitroValidator: keys must be the contiguous set 0..count-1.
        for &key in &seen {
            if key >= pcrs.value {
                return Err(PlannerError::Cose(format!(
                    "PCR key {key} is out of range for {} entries",
                    pcrs.value
                )));
            }
        }
        Ok(())
    }

    /// Validates cabundle length and per-certificate size limits.
    pub fn validate_cabundle(payload: &[u8], cabundle: CborItem) -> PlannerResult<()> {
        let cabundle = cabundle.require_major(CBOR_MAJOR_ARRAY, "cabundle")?;
        if cabundle.indefinite {
            return Err(PlannerError::Cose("cabundle must be definite-length".into()));
        }
        if cabundle.value == 0 || cabundle.value > MAX_CABUNDLE_CERTS as u64 {
            return Err(PlannerError::Cose(format!(
                "cabundle has {} certificates, must be 1-{MAX_CABUNDLE_CERTS}",
                cabundle.value
            )));
        }

        let mut offset = cabundle.content_start;
        for i in 0..cabundle.value {
            let item = CborItem::read(payload, offset)?
                .require_major(CBOR_MAJOR_BYTE_STRING, "cabundle certificate")?;
            let len = item.end - item.content_start;
            if len == 0 || len > MAX_CABUNDLE_CERT_BYTES {
                return Err(PlannerError::Cose(format!(
                    "cabundle[{i}] must be between 1 and {MAX_CABUNDLE_CERT_BYTES} bytes, got {len}"
                )));
            }
            offset = item.end;
        }
        Ok(())
    }
}
