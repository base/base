//! PCK certificate platform identity and TCB extension extraction.

use alloy_primitives::Bytes;
use x509_parser::{
    certificate::X509Certificate,
    der_parser::{
        Oid,
        der::{DerObjectContent, parse_der},
        oid,
    },
    prelude::FromDer,
};

use crate::{Result, TdxVerifierError};

use super::CollateralVerifier;

const INTEL_SGX_EXTENSION_OID: Oid<'static> = oid!(1.2.840.113741.1.13.1);
const INTEL_TCB_COMPONENT_PREFIX_OID: Oid<'static> = oid!(1.2.840.113741.1.13.1.2);
const INTEL_PCE_SVN_OID: Oid<'static> = oid!(1.2.840.113741.1.13.1.2.17);
const INTEL_PCE_ID_OID: Oid<'static> = oid!(1.2.840.113741.1.13.1.3);
const INTEL_FMSPC_OID: Oid<'static> = oid!(1.2.840.113741.1.13.1.4);

/// Platform identity fields authenticated by the PCK certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdxPlatformIdentity {
    /// Intel FMSPC bytes for the platform.
    pub fmspc: Bytes,
    /// Intel PCE ID bytes for the platform.
    pub pce_id: Bytes,
}

impl TdxPlatformIdentity {
    /// Extracts Intel platform identity and SGX/PCE TCB extensions from one PCK parse.
    pub fn platform_and_tcb_from_pck_certificate_der(raw: &[u8]) -> Result<(Self, TdxPckTcb)> {
        let (_, cert) = X509Certificate::from_der(raw).map_err(|e| {
            TdxVerifierError::PckCertChainInvalid(format!("X.509 parse failed: {e}"))
        })?;
        let mut fmspc = None;
        let mut pce_id = None;
        let mut sgx_tcb_svn = [0u8; 16];
        let mut sgx_tcb_seen = [false; 16];
        let mut pce_svn = None;

        let extension = cert
            .tbs_certificate
            .get_extension_unique(&INTEL_SGX_EXTENSION_OID)
            .map_err(|e| {
                TdxVerifierError::PckCertChainInvalid(format!(
                    "Intel SGX extension lookup failed: {e}"
                ))
            })?
            .ok_or_else(|| {
                TdxVerifierError::PckCertChainInvalid(
                    "PCK certificate is missing Intel SGX extension".into(),
                )
            })?;
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
            let oid = oid.as_bytes();

            if oid == INTEL_PCE_ID_OID.as_bytes() {
                let DerObjectContent::OctetString(content) = value_object.content else {
                    return Err(TdxVerifierError::PckCertChainInvalid(
                        "PCE ID is not encoded as DER OCTET STRING".into(),
                    ));
                };
                pce_id = Some(Bytes::copy_from_slice(content));
            } else if oid == INTEL_FMSPC_OID.as_bytes() {
                let DerObjectContent::OctetString(content) = value_object.content else {
                    return Err(TdxVerifierError::PckCertChainInvalid(
                        "FMSPC is not encoded as DER OCTET STRING".into(),
                    ));
                };
                fmspc = Some(Bytes::copy_from_slice(content));
            } else if oid == INTEL_PCE_SVN_OID.as_bytes() {
                let value = value_object.as_u64().map_err(|e| {
                    TdxVerifierError::PckCertChainInvalid(format!(
                        "PCE SVN is not an unsigned DER INTEGER: {e:?}"
                    ))
                })?;
                pce_svn = Some(u16::try_from(value).map_err(|_| {
                    TdxVerifierError::PckCertChainInvalid(
                        "PCK certificate PCE SVN exceeds u16".into(),
                    )
                })?);
            } else if let Some(suffix) = oid.strip_prefix(INTEL_TCB_COMPONENT_PREFIX_OID.as_bytes())
            {
                let [component] = suffix else {
                    continue;
                };
                if !(1..=16).contains(component) {
                    continue;
                }
                let component_index = usize::from(*component) - 1;
                let value = value_object.as_u64().map_err(|e| {
                    TdxVerifierError::PckCertChainInvalid(format!(
                        "SGX TCB component is not an unsigned DER INTEGER: {e:?}"
                    ))
                })?;
                sgx_tcb_svn[component_index] = u8::try_from(value).map_err(|_| {
                    TdxVerifierError::PckCertChainInvalid(format!(
                        "PCK certificate SGX TCB component {} exceeds u8",
                        component_index + 1
                    ))
                })?;
                sgx_tcb_seen[component_index] = true;
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
}

/// SGX/PCE TCB values authenticated by the PCK certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdxPckTcb {
    /// SGX CPU SVN component values authenticated by the PCK certificate.
    pub sgx_tcb_svn: [u8; 16],
    /// PCE SVN authenticated by the PCK certificate.
    pub pce_svn: u16,
}
