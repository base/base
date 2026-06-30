//! PCK certificate platform identity and TCB extension extraction.

use alloy_primitives::Bytes;
use x509_parser::{certificate::X509Certificate, prelude::FromDer};

use crate::{Result, TdxVerifierError};

use super::CollateralVerifier;

/// Platform identity fields authenticated by the PCK certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdxPlatformIdentity {
    /// Intel FMSPC bytes for the platform.
    pub fmspc: Bytes,
    /// Intel PCE ID bytes for the platform.
    pub pce_id: Bytes,
}

impl TdxPlatformIdentity {
    /// Extracts Intel platform identity extensions from an authenticated PCK certificate.
    pub fn from_pck_certificate_der(raw: &[u8]) -> Result<Self> {
        let (_, cert) = X509Certificate::from_der(raw).map_err(|e| {
            TdxVerifierError::PckCertChainInvalid(format!("X.509 parse failed: {e}"))
        })?;
        let mut fmspc = None;
        let mut pce_id = None;

        for extension in cert.tbs_certificate.extensions() {
            if fmspc.is_some() && pce_id.is_some() {
                break;
            }
            match extension.oid.to_id_string().as_str() {
                "1.2.840.113741.1.13.1.3" => {
                    pce_id = Some(
                        Self::decode_extension_octets(extension.value)
                            .map_err(TdxVerifierError::PckCertChainInvalid)?,
                    );
                }
                "1.2.840.113741.1.13.1.4" => {
                    fmspc = Some(
                        Self::decode_extension_octets(extension.value)
                            .map_err(TdxVerifierError::PckCertChainInvalid)?,
                    );
                }
                _ => {
                    if fmspc.is_none() {
                        fmspc = Self::find_nested_oid_octets(
                            extension.value,
                            "1.2.840.113741.1.13.1.4",
                        );
                    }
                    if pce_id.is_none() {
                        pce_id = Self::find_nested_oid_octets(
                            extension.value,
                            "1.2.840.113741.1.13.1.3",
                        );
                    }
                }
            }
        }

        Ok(Self {
            fmspc: fmspc.ok_or_else(|| {
                TdxVerifierError::PckCertChainInvalid("PCK certificate is missing FMSPC".into())
            })?,
            pce_id: pce_id.ok_or_else(|| {
                TdxVerifierError::PckCertChainInvalid("PCK certificate is missing PCE ID".into())
            })?,
        })
    }

    /// Builds a platform identity from signed TCB info JSON hex fields.
    pub fn from_tcb_info(fmspc: &str, pce_id: &str) -> Result<Self> {
        Ok(Self {
            fmspc: CollateralVerifier::decode_hex(fmspc)
                .map_err(TdxVerifierError::TcbInfoInvalid)?,
            pce_id: CollateralVerifier::decode_hex(pce_id)
                .map_err(TdxVerifierError::TcbInfoInvalid)?,
        })
    }

    /// Reads an OCTET STRING extension payload if one wraps the platform bytes.
    pub fn decode_extension_octets(value: &[u8]) -> std::result::Result<Bytes, String> {
        if let Some((tag, content, end)) = CollateralVerifier::read_der_tlv(value, 0)
            && tag == 0x04
            && end == value.len()
        {
            return Ok(Bytes::copy_from_slice(content));
        }
        Ok(Bytes::copy_from_slice(value))
    }

    /// Finds a nested OID followed by an OCTET STRING payload inside Intel SGX extension data.
    pub fn find_nested_oid_octets(value: &[u8], target_oid: &str) -> Option<Bytes> {
        Self::find_nested_oid_value(value, target_oid).and_then(|(tag, content)| {
            if tag == 0x04 { Some(Bytes::copy_from_slice(content)) } else { None }
        })
    }

    /// Finds a nested OID followed by an unsigned INTEGER payload inside Intel SGX extension data.
    pub fn find_nested_oid_integer(
        value: &[u8],
        target_oid: &str,
    ) -> std::result::Result<Option<u64>, String> {
        Self::find_nested_oid_value(value, target_oid)
            .map(|(tag, content)| {
                if tag != 0x02 {
                    return Err(format!("{target_oid} is not encoded as DER INTEGER"));
                }
                Self::decode_der_unsigned_integer(content)
            })
            .transpose()
    }

    /// Finds a nested OID followed by any DER value inside Intel SGX extension data.
    pub fn find_nested_oid_value<'a>(value: &'a [u8], target_oid: &str) -> Option<(u8, &'a [u8])> {
        let mut offset = 0;
        while offset < value.len() {
            let (tag, content, end) = CollateralVerifier::read_der_tlv(value, offset)?;
            if tag == 0x06
                && CollateralVerifier::decode_der_oid(content).as_deref() == Some(target_oid)
                && let Some((next_tag, next_content, _)) =
                    CollateralVerifier::read_der_tlv(value, end)
            {
                return Some((next_tag, next_content));
            }
            if tag & 0x20 != 0
                && let Some(nested) = Self::find_nested_oid_value(content, target_oid)
            {
                return Some(nested);
            }
            offset = end;
        }
        None
    }

    /// Decodes a non-negative DER INTEGER body into an unsigned integer.
    pub fn decode_der_unsigned_integer(content: &[u8]) -> std::result::Result<u64, String> {
        if content.is_empty() {
            return Err("DER INTEGER is empty".into());
        }
        if content[0] & 0x80 != 0 {
            return Err("DER INTEGER is negative".into());
        }

        let significant_len = content.iter().skip_while(|byte| **byte == 0).count();
        if significant_len > std::mem::size_of::<u64>() {
            return Err("DER INTEGER exceeds u64".into());
        }

        Ok(content
            .iter()
            .skip_while(|byte| **byte == 0)
            .fold(0u64, |value, byte| (value << 8) | u64::from(*byte)))
    }
}

/// SGX/PCE TCB values authenticated by the PCK certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdxPckTcb {
    /// SGX CPU SVN component values authenticated by the PCK certificate.
    pub sgx_tcb_svn: [u8; 16],
    /// PCE SVN authenticated by the PCK certificate.
    pub pce_svn: u16,
}

impl TdxPckTcb {
    /// Extracts Intel SGX/PCE TCB extensions from an authenticated PCK certificate.
    pub fn from_pck_certificate_der(raw: &[u8]) -> Result<Self> {
        let (_, cert) = X509Certificate::from_der(raw).map_err(|e| {
            TdxVerifierError::PckCertChainInvalid(format!("X.509 parse failed: {e}"))
        })?;
        let mut sgx_tcb_svn = [0u8; 16];
        let mut sgx_tcb_seen = [false; 16];
        let mut pce_svn = None;

        for extension in cert.tbs_certificate.extensions() {
            if sgx_tcb_seen.iter().all(|seen| *seen) && pce_svn.is_some() {
                break;
            }
            for component_index in 0..sgx_tcb_svn.len() {
                if sgx_tcb_seen[component_index] {
                    continue;
                }

                let oid = format!("1.2.840.113741.1.13.1.2.{}", component_index + 1);
                if let Some(value) =
                    TdxPlatformIdentity::find_nested_oid_integer(extension.value, &oid)
                        .map_err(TdxVerifierError::PckCertChainInvalid)?
                {
                    sgx_tcb_svn[component_index] = u8::try_from(value).map_err(|_| {
                        TdxVerifierError::PckCertChainInvalid(format!(
                            "PCK certificate SGX TCB component {} exceeds u8",
                            component_index + 1
                        ))
                    })?;
                    sgx_tcb_seen[component_index] = true;
                }
            }

            if pce_svn.is_none() {
                pce_svn = TdxPlatformIdentity::find_nested_oid_integer(
                    extension.value,
                    "1.2.840.113741.1.13.1.2.17",
                )
                .map_err(TdxVerifierError::PckCertChainInvalid)?
                .map(|value| {
                    u16::try_from(value).map_err(|_| {
                        TdxVerifierError::PckCertChainInvalid(
                            "PCK certificate PCE SVN exceeds u16".into(),
                        )
                    })
                })
                .transpose()?;
            }
        }

        if sgx_tcb_seen.iter().any(|seen| !seen) {
            return Err(TdxVerifierError::PckCertChainInvalid(
                "PCK certificate is missing SGX TCB components".into(),
            ));
        }

        Ok(Self {
            sgx_tcb_svn,
            pce_svn: pce_svn.ok_or_else(|| {
                TdxVerifierError::PckCertChainInvalid("PCK certificate is missing PCE SVN".into())
            })?,
        })
    }
}
