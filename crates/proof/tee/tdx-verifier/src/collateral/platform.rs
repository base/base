//! PCK certificate platform identity and TCB extension extraction.

use alloy_primitives::Bytes;
use x509_parser::{
    certificate::X509Certificate,
    der_parser::der::{DerObject, DerObjectContent, parse_der},
    prelude::FromDer,
};

use crate::{Result, TdxVerifierError};

use super::CollateralVerifier;

const INTEL_SGX_EXTENSION_OID: &str = "1.2.840.113741.1.13.1";

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
        Self::platform_and_tcb_from_pck_certificate_der(raw).map(|(platform, _)| platform)
    }

    /// Extracts Intel platform identity and SGX/PCE TCB extensions from one PCK parse.
    pub fn platform_and_tcb_from_pck_certificate_der(raw: &[u8]) -> Result<(Self, TdxPckTcb)> {
        let (_, cert) = X509Certificate::from_der(raw).map_err(|e| {
            TdxVerifierError::PckCertChainInvalid(format!("X.509 parse failed: {e}"))
        })?;
        Self::platform_and_tcb_from_pck_certificate(&cert)
    }

    /// Extracts Intel platform identity and SGX/PCE TCB extensions from an authenticated PCK cert.
    pub fn platform_and_tcb_from_pck_certificate(
        cert: &X509Certificate<'_>,
    ) -> Result<(Self, TdxPckTcb)> {
        let mut fmspc = None;
        let mut pce_id = None;
        let mut sgx_tcb_svn = [0u8; 16];
        let mut sgx_tcb_seen = [false; 16];
        let mut pce_svn = None;

        for extension in cert.tbs_certificate.extensions() {
            if fmspc.is_some()
                && pce_id.is_some()
                && sgx_tcb_seen.iter().all(|seen| *seen)
                && pce_svn.is_some()
            {
                break;
            }

            if extension.oid.to_id_string() != INTEL_SGX_EXTENSION_OID {
                continue;
            }

            let (_, sgx_extension) = parse_der(extension.value).map_err(|e| {
                TdxVerifierError::PckCertChainInvalid(format!(
                    "Intel SGX extension parse failed: {e:?}"
                ))
            })?;
            let DerObjectContent::Sequence(entries) = sgx_extension.content else {
                return Err(TdxVerifierError::PckCertChainInvalid(
                    "Intel SGX extension is not a DER SEQUENCE".into(),
                ));
            };

            for entry in entries {
                let DerObjectContent::Sequence(fields) = entry.content else {
                    continue;
                };
                let [oid_object, value_object] = fields.as_slice() else {
                    continue;
                };
                let Ok(oid) = oid_object.as_oid() else {
                    continue;
                };
                let Some(arcs) = oid.iter().map(|iter| iter.collect::<Vec<_>>()) else {
                    continue;
                };

                match arcs.as_slice() {
                    [1, 2, 840, 113741, 1, 13, 1, 3] => {
                        pce_id = Some(
                            Self::octets_from_der_object(value_object, "PCE ID")
                                .map_err(TdxVerifierError::PckCertChainInvalid)?,
                        );
                    }
                    [1, 2, 840, 113741, 1, 13, 1, 4] => {
                        fmspc = Some(
                            Self::octets_from_der_object(value_object, "FMSPC")
                                .map_err(TdxVerifierError::PckCertChainInvalid)?,
                        );
                    }
                    [1, 2, 840, 113741, 1, 13, 1, 2, component] if (1..=16).contains(component) => {
                        let component_index = (*component as usize) - 1;
                        let value = Self::unsigned_integer_from_der_object(
                            value_object,
                            "SGX TCB component",
                        )
                        .map_err(TdxVerifierError::PckCertChainInvalid)?;
                        sgx_tcb_svn[component_index] = u8::try_from(value).map_err(|_| {
                            TdxVerifierError::PckCertChainInvalid(format!(
                                "PCK certificate SGX TCB component {} exceeds u8",
                                component_index + 1
                            ))
                        })?;
                        sgx_tcb_seen[component_index] = true;
                    }
                    [1, 2, 840, 113741, 1, 13, 1, 2, 17] => {
                        let value = Self::unsigned_integer_from_der_object(value_object, "PCE SVN")
                            .map_err(TdxVerifierError::PckCertChainInvalid)?;
                        pce_svn = Some(u16::try_from(value).map_err(|_| {
                            TdxVerifierError::PckCertChainInvalid(
                                "PCK certificate PCE SVN exceeds u16".into(),
                            )
                        })?);
                    }
                    _ => {}
                }
            }
        }

        if sgx_tcb_seen.iter().any(|seen| !seen) {
            return Err(TdxVerifierError::PckCertChainInvalid(
                "PCK certificate is missing SGX TCB components".into(),
            ));
        }

        let platform = Self {
            fmspc: fmspc.ok_or_else(|| {
                TdxVerifierError::PckCertChainInvalid("PCK certificate is missing FMSPC".into())
            })?,
            pce_id: pce_id.ok_or_else(|| {
                TdxVerifierError::PckCertChainInvalid("PCK certificate is missing PCE ID".into())
            })?,
        };
        let tcb = TdxPckTcb {
            sgx_tcb_svn,
            pce_svn: pce_svn.ok_or_else(|| {
                TdxVerifierError::PckCertChainInvalid("PCK certificate is missing PCE SVN".into())
            })?,
        };

        Ok((platform, tcb))
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

    /// Reads an OCTET STRING value from a parsed DER object.
    pub fn octets_from_der_object(
        object: &DerObject<'_>,
        name: &str,
    ) -> std::result::Result<Bytes, String> {
        if let DerObjectContent::OctetString(content) = object.content {
            return Ok(Bytes::copy_from_slice(content));
        }
        Err(format!("{name} is not encoded as DER OCTET STRING"))
    }

    /// Reads an unsigned INTEGER value from a parsed DER object.
    pub fn unsigned_integer_from_der_object(
        object: &DerObject<'_>,
        name: &str,
    ) -> std::result::Result<u64, String> {
        object.as_u64().map_err(|e| format!("{name} is not an unsigned DER INTEGER: {e:?}"))
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
        TdxPlatformIdentity::platform_and_tcb_from_pck_certificate_der(raw).map(|(_, tcb)| tcb)
    }
}
