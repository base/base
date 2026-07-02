//! TDX attestation collateral hydration for proof generation.

use std::error::Error;

use alloy_primitives::{Bytes, hex};
use base_proof_tee_tdx_verifier::{
    CollateralVerifier, TdxCertificate, TdxCollateral, TdxPlatformIdentity, TdxQuote,
    TdxRevocationEvidence, TdxSignedCollateral, TdxSignedCollateralBody,
};
use percent_encoding::percent_decode_str;
use reqwest::Url;
use x509_parser::{
    certificate::X509Certificate,
    extensions::{DistributionPointName, GeneralName, ParsedExtension},
    pem::Pem,
    prelude::FromDer,
};

use crate::TdxAttestationConfig;

/// Maximum allowed Intel PCS response size.
const MAX_TDX_COLLATERAL_RESPONSE_BYTES: usize = 10 * 1024 * 1024;

const PCK_CERT_CHAIN_CERTIFICATION_DATA_TYPE: u16 = 5;
const TCB_INFO_ISSUER_CHAIN_HEADERS: &[&str] =
    &["tcb-info-issuer-chain", "sgx-tcb-info-issuer-chain"];
const TCB_INFO_SIGNATURE_HEADERS: &[&str] = &["tcb-info-signature", "sgx-tcb-info-signature"];
const QE_IDENTITY_ISSUER_CHAIN_HEADERS: &[&str] = &["sgx-enclave-identity-issuer-chain"];

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
    config: TdxAttestationConfig,
    client: reqwest::Client,
}

impl TdxAttestationHydrator {
    /// Creates a hydrator with a hardened HTTP client.
    pub fn new(config: TdxAttestationConfig) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let client = reqwest::Client::builder()
            .timeout(config.fetch_timeout)
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()?;
        Ok(Self { config, client })
    }

    /// Fetches Intel PCS collateral and CRLs required to verify `quote`.
    pub async fn fetch_collateral(
        &self,
        quote: &[u8],
    ) -> Result<TdxCollateralFetch, Box<dyn Error + Send + Sync>> {
        let parsed_quote = TdxQuote::parse(quote)?;
        if parsed_quote.certification_data_type != PCK_CERT_CHAIN_CERTIFICATION_DATA_TYPE {
            return Err(format!(
                "unsupported TDX quote certification data type {}",
                parsed_quote.certification_data_type,
            )
            .into());
        }
        let pck_certificate_chain =
            Self::certificate_chain_from_pem(&parsed_quote.certification_data)?;
        let pck_leaf =
            pck_certificate_chain.last().expect("certificate_chain_from_pem rejects empty chains");
        let (platform, _) =
            TdxPlatformIdentity::platform_and_tcb_from_pck_certificate_der(&pck_leaf.raw)?;

        let mut tcb_info_url = self.config.pcs_tdx_base_url.join("tcb")?;
        tcb_info_url
            .query_pairs_mut()
            .append_pair("fmspc", &hex::encode(&platform.fmspc))
            .append_pair("pceid", &hex::encode(&platform.pce_id));
        let qe_identity_url = self.config.pcs_tdx_base_url.join("qe/identity")?;
        let (tcb_info, qe_identity) = tokio::try_join!(
            self.fetch_signed_collateral(
                tcb_info_url,
                TCB_INFO_ISSUER_CHAIN_HEADERS,
                Some(TCB_INFO_SIGNATURE_HEADERS),
            ),
            self.fetch_signed_collateral(qe_identity_url, QE_IDENTITY_ISSUER_CHAIN_HEADERS, None),
        )?;
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

    async fn fetch_signed_collateral(
        &self,
        url: Url,
        chain_headers: &[&str],
        signature_headers: Option<&[&str]>,
    ) -> Result<TdxSignedCollateral, Box<dyn Error + Send + Sync>> {
        let response = self.send(url).await?;
        let headers = response.headers().clone();
        let raw = Self::limited_body(response).await?;
        let encoded_chain = chain_headers
            .iter()
            .find_map(|header| headers.get(*header))
            .ok_or_else(|| format!("Intel PCS response missing {}", chain_headers.join(" or ")))?
            .to_str()?;
        let decoded_chain = percent_decode_str(encoded_chain).decode_utf8()?;
        let signing_chain = Self::certificate_chain_from_pem(decoded_chain.as_bytes())?;
        let signature = match signature_headers
            .and_then(|header_names| header_names.iter().find_map(|header| headers.get(*header)))
        {
            Some(value) => CollateralVerifier::decode_hex(value.to_str()?.trim())?,
            None => {
                TdxSignedCollateral::signature_from_json(&raw, TdxSignedCollateralBody::QeIdentity)?
            }
        };
        Ok(TdxSignedCollateral { raw, signing_chain, signature })
    }

    async fn fetch_revocation_evidence(
        &self,
        chains: &[&[TdxCertificate]],
    ) -> Result<TdxRevocationEvidence, Box<dyn Error + Send + Sync>> {
        let mut urls = Vec::new();
        for chain in chains {
            for certificate in chain.iter().skip(1) {
                let url = Self::crl_distribution_point(&certificate.raw)?;
                if !urls.contains(&url) {
                    urls.push(url);
                }
            }
        }
        let mut certificate_crls = Vec::with_capacity(urls.len());
        for url in urls {
            let response = self.send(url).await?;
            certificate_crls.push(Self::limited_body(response).await?);
        }
        Ok(TdxRevocationEvidence { certificate_crls })
    }

    async fn send(&self, url: Url) -> Result<reqwest::Response, Box<dyn Error + Send + Sync>> {
        self.client
            .get(url)
            .send()
            .await
            .and_then(|response| response.error_for_status())
            .map_err(Into::into)
    }

    async fn limited_body(
        response: reqwest::Response,
    ) -> Result<Bytes, Box<dyn Error + Send + Sync>> {
        if response
            .content_length()
            .is_some_and(|len| len > MAX_TDX_COLLATERAL_RESPONSE_BYTES as u64)
        {
            return Err("Intel PCS response exceeds size limit".into());
        }
        let bytes = response.bytes().await?;
        if bytes.len() > MAX_TDX_COLLATERAL_RESPONSE_BYTES {
            return Err("Intel PCS response exceeds size limit".into());
        }
        Ok(Bytes(bytes))
    }

    fn certificate_chain_from_pem(
        pem_bytes: &[u8],
    ) -> Result<Vec<TdxCertificate>, Box<dyn Error + Send + Sync>> {
        let mut chain = Vec::new();
        for pem in Pem::iter_from_buffer(pem_bytes) {
            let pem = pem.map_err(|e| format!("PEM parse failed: {e}"))?;
            if pem.label == "CERTIFICATE" {
                chain.push(TdxCertificate { raw: Bytes::from(pem.contents) });
            }
        }
        if chain.is_empty() {
            return Err("certificate chain is empty".into());
        }
        chain.reverse();
        Ok(chain)
    }

    fn crl_distribution_point(certificate_der: &[u8]) -> Result<Url, Box<dyn Error + Send + Sync>> {
        let (_, certificate) = X509Certificate::from_der(certificate_der)?;
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
                        let url = Url::parse(uri)?;
                        if !url.host_str().is_some_and(|host| {
                            let host = host.to_ascii_lowercase();
                            host == "trustedservices.intel.com"
                                || host.ends_with(".trustedservices.intel.com")
                        }) {
                            return Err(format!(
                                "TDX certificate CRL URL is not an allowed Intel URL: {uri}"
                            )
                            .into());
                        }
                        return Ok(url);
                    }
                }
            }
        }
        Err("certificate is missing HTTPS CRL distribution point".into())
    }
}
