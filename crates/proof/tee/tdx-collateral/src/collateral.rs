//! TDX attestation collateral hydration for proof generation.

use alloy_primitives::{Bytes, hex};
use base_proof_tee_tdx_verifier::{
    CollateralVerifier, TdxCertificate, TdxCollateral, TdxPlatformIdentity, TdxQuote,
    TdxRevocationEvidence, TdxSignedCollateral,
};
use percent_encoding::percent_decode_str;
use reqwest::Url;
use x509_parser::{
    certificate::X509Certificate,
    extensions::{DistributionPointName, GeneralName, ParsedExtension},
    pem::Pem,
    prelude::FromDer,
};

use crate::{Result, TdxAttestationConfig, TdxCollateralError};

/// Maximum allowed Intel PCS response size.
const MAX_TDX_COLLATERAL_RESPONSE_BYTES: usize = 10 * 1024 * 1024;

const PCK_CERT_CHAIN_CERTIFICATION_DATA_TYPE: u16 = 5;
const TCB_INFO_ISSUER_CHAIN_HEADER: &str = "tcb-info-issuer-chain";
const LEGACY_TCB_INFO_ISSUER_CHAIN_HEADER: &str = "sgx-tcb-info-issuer-chain";
const TCB_INFO_SIGNATURE_HEADER: &str = "tcb-info-signature";
const LEGACY_TCB_INFO_SIGNATURE_HEADER: &str = "sgx-tcb-info-signature";
const QE_IDENTITY_ISSUER_CHAIN_HEADER: &str = "sgx-enclave-identity-issuer-chain";

/// TDX collateral fetched from Intel PCS for one signer quote.
#[derive(Debug, Clone)]
pub struct TdxCollateralFetch {
    /// Root-to-leaf PCK certificate chain carried by the quote.
    pub pck_certificate_chain: Vec<TdxCertificate>,
    /// TCB info and QE identity collateral.
    pub collateral: TdxCollateral,
    /// CRLs covering non-root certificates in the verifier input.
    pub revocation: TdxRevocationEvidence,
}

/// Fetches Intel PCS collateral for TDX quote verification.
#[derive(Debug)]
pub struct TdxAttestationHydrator {
    /// Intel PCS and verifier policy configuration.
    pub config: TdxAttestationConfig,
    client: reqwest::Client,
}

impl TdxAttestationHydrator {
    /// Creates a hydrator with a hardened HTTP client.
    pub fn new(config: TdxAttestationConfig) -> Result<Self> {
        let client = config.build_http_client()?;
        Ok(Self { config, client })
    }

    /// Fetches Intel PCS collateral and CRLs required to verify `quote`.
    pub async fn fetch_collateral(&self, quote: &[u8]) -> Result<TdxCollateralFetch> {
        let parsed_quote = TdxQuote::parse(quote).map_err(|e| TdxCollateralError::source(e))?;
        if parsed_quote.certification_data_type != PCK_CERT_CHAIN_CERTIFICATION_DATA_TYPE {
            return Err(TdxCollateralError::source(format!(
                "unsupported TDX quote certification data type {}",
                parsed_quote.certification_data_type,
            )));
        }
        let pck_certificate_chain =
            Self::certificate_chain_from_pem(&parsed_quote.certification_data)?;
        let pck_leaf = pck_certificate_chain
            .last()
            .ok_or_else(|| TdxCollateralError::source("PCK certificate chain is empty"))?;
        let (platform, _) =
            TdxPlatformIdentity::platform_and_tcb_from_pck_certificate_der(&pck_leaf.raw)
                .map_err(|e| TdxCollateralError::source(e))?;

        let (tcb_info, qe_identity) =
            tokio::try_join!(self.fetch_tcb_info(&platform), self.fetch_qe_identity())?;
        let collateral = TdxCollateral { tcb_info, qe_identity };
        let revocation = self
            .fetch_revocation_evidence(&[
                pck_certificate_chain.as_slice(),
                collateral.tcb_info.signing_chain.as_slice(),
                collateral.qe_identity.signing_chain.as_slice(),
            ])
            .await?;
        Ok(TdxCollateralFetch { pck_certificate_chain, collateral, revocation })
    }

    async fn fetch_tcb_info(&self, platform: &TdxPlatformIdentity) -> Result<TdxSignedCollateral> {
        let mut url =
            self.config.pcs_tdx_base_url.join("tcb").map_err(|e| TdxCollateralError::source(e))?;
        url.query_pairs_mut()
            .append_pair("fmspc", &hex::encode(&platform.fmspc))
            .append_pair("pceid", &hex::encode(&platform.pce_id));
        let chain_headers = [TCB_INFO_ISSUER_CHAIN_HEADER, LEGACY_TCB_INFO_ISSUER_CHAIN_HEADER];
        let signature_headers = [TCB_INFO_SIGNATURE_HEADER, LEGACY_TCB_INFO_SIGNATURE_HEADER];
        self.fetch_signed_collateral(url, &chain_headers, Some(&signature_headers)).await
    }

    async fn fetch_qe_identity(&self) -> Result<TdxSignedCollateral> {
        let url = self
            .config
            .pcs_tdx_base_url
            .join("qe/identity")
            .map_err(|e| TdxCollateralError::source(e))?;
        let chain_headers = [QE_IDENTITY_ISSUER_CHAIN_HEADER];
        self.fetch_signed_collateral(url, &chain_headers, None).await
    }

    async fn fetch_signed_collateral(
        &self,
        url: Url,
        chain_headers: &[&str],
        signature_headers: Option<&[&str]>,
    ) -> Result<TdxSignedCollateral> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .and_then(|response| response.error_for_status())
            .map_err(|e| TdxCollateralError::source(e))?;
        let headers = response.headers().clone();
        let raw = Self::limited_body(response).await?;
        let encoded_chain = chain_headers
            .iter()
            .find_map(|header| headers.get(*header))
            .ok_or_else(|| {
                TdxCollateralError::source(format!(
                    "Intel PCS response missing {}",
                    chain_headers.join(" or ")
                ))
            })?
            .to_str()
            .map_err(|e| TdxCollateralError::source(e))?;
        let decoded_chain = percent_decode_str(encoded_chain)
            .decode_utf8()
            .map_err(|e| TdxCollateralError::source(e))?
            .into_owned()
            .into_bytes();
        let signing_chain = Self::certificate_chain_from_pem(&decoded_chain)?;
        let signature = match signature_headers
            .and_then(|header_names| header_names.iter().find_map(|header| headers.get(*header)))
        {
            Some(value) => CollateralVerifier::decode_hex(
                value.to_str().map_err(|e| TdxCollateralError::source(e))?.trim(),
            )
            .map_err(TdxCollateralError::source)?,
            None => Self::qe_identity_signature_from_json(&raw)?,
        };
        Ok(TdxSignedCollateral { raw, signing_chain, signature })
    }

    async fn fetch_revocation_evidence(
        &self,
        chains: &[&[TdxCertificate]],
    ) -> Result<TdxRevocationEvidence> {
        let mut urls = Vec::new();
        for chain in chains {
            for certificate in chain.iter().skip(1) {
                let crl_url = Self::crl_distribution_point(&certificate.raw)?;
                let url = Url::parse(&crl_url).map_err(|e| TdxCollateralError::source(e))?;
                if !Self::is_allowed_intel_url(&url) {
                    return Err(TdxCollateralError::source(format!(
                        "TDX certificate CRL URL is not an allowed Intel URL: {crl_url}"
                    )));
                }
                if !urls.contains(&url) {
                    urls.push(url);
                }
            }
        }
        let mut certificate_crls = Vec::with_capacity(urls.len());
        for url in urls {
            let response = self
                .client
                .get(url)
                .send()
                .await
                .and_then(|response| response.error_for_status())
                .map_err(|e| TdxCollateralError::source(e))?;
            certificate_crls.push(Self::limited_body(response).await?);
        }
        Ok(TdxRevocationEvidence { certificate_crls })
    }

    async fn limited_body(response: reqwest::Response) -> Result<Bytes> {
        if response
            .content_length()
            .is_some_and(|len| len > MAX_TDX_COLLATERAL_RESPONSE_BYTES as u64)
        {
            return Err(TdxCollateralError::source("Intel PCS response exceeds size limit"));
        }
        let bytes = response.bytes().await.map_err(|e| TdxCollateralError::source(e))?;
        if bytes.len() > MAX_TDX_COLLATERAL_RESPONSE_BYTES {
            return Err(TdxCollateralError::source("Intel PCS response exceeds size limit"));
        }
        Ok(Bytes(bytes))
    }

    fn qe_identity_signature_from_json(raw: &[u8]) -> Result<Bytes> {
        let document: serde_json::Value =
            serde_json::from_slice(raw).map_err(|e| TdxCollateralError::source(e))?;
        let value = document
            .get("signature")
            .ok_or_else(|| {
                TdxCollateralError::source("Intel PCS response missing QE identity signature")
            })?
            .as_str()
            .ok_or_else(|| {
                TdxCollateralError::source(
                    "Intel PCS response QE identity signature is not a string",
                )
            })?;
        CollateralVerifier::decode_hex(value.trim()).map_err(TdxCollateralError::source)
    }

    fn certificate_chain_from_pem(pem_bytes: &[u8]) -> Result<Vec<TdxCertificate>> {
        let mut chain = Vec::new();
        for pem in Pem::iter_from_buffer(pem_bytes) {
            let pem =
                pem.map_err(|e| TdxCollateralError::source(format!("PEM parse failed: {e}")))?;
            if pem.label == "CERTIFICATE" {
                chain.push(TdxCertificate { raw: Bytes::from(pem.contents) });
            }
        }
        if chain.is_empty() {
            return Err(TdxCollateralError::source("certificate chain is empty"));
        }
        chain.reverse();
        Ok(chain)
    }

    fn crl_distribution_point(certificate_der: &[u8]) -> Result<String> {
        let (_, certificate) = X509Certificate::from_der(certificate_der)
            .map_err(|e| TdxCollateralError::source(e))?;
        for extension in certificate.extensions() {
            let ParsedExtension::CRLDistributionPoints(points) = extension.parsed_extension()
            else {
                continue;
            };
            for point in points.iter() {
                let Some(DistributionPointName::FullName(names)) = &point.distribution_point else {
                    continue;
                };
                for name in names {
                    let GeneralName::URI(uri) = name else { continue };
                    if uri.starts_with("https://") {
                        return Ok(uri.to_string());
                    }
                }
            }
        }
        Err(TdxCollateralError::source("certificate is missing HTTPS CRL distribution point"))
    }

    fn is_allowed_intel_url(url: &Url) -> bool {
        url.scheme() == "https"
            && url.host_str().is_some_and(|host| {
                let host = host.to_ascii_lowercase();
                host == "trustedservices.intel.com" || host.ends_with(".trustedservices.intel.com")
            })
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;

    fn assert_source_contains(error: TdxCollateralError, expected: &str) {
        let source = error.source().expect("error should retain source").to_string();
        assert!(source.contains(expected), "{source}");
    }

    #[test]
    fn qe_identity_signature_from_json_body_decodes_top_level_signature() {
        let raw = br#"{"enclaveIdentity":{},"signature":"0x0102ff"}"#;

        let signature = TdxAttestationHydrator::qe_identity_signature_from_json(raw).unwrap();

        assert_eq!(signature, Bytes::from_static(&[0x01, 0x02, 0xff]));
    }

    #[test]
    fn qe_identity_signature_from_json_body_requires_signature_field() {
        let raw = br#"{"enclaveIdentity":{}}"#;

        let error = TdxAttestationHydrator::qe_identity_signature_from_json(raw).unwrap_err();

        assert_source_contains(error, "Intel PCS response missing QE identity signature");
    }
}
