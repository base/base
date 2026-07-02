//! TDX attestation collateral hydration for proof generation.

use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{Address, B256, Bytes, hex};
use base_proof_tee_tdx_attestation_prover::TdxAttestationProverInput;
use base_proof_tee_tdx_verifier::{
    AuthenticatedTdxCertificate, ParsedTdxQuote, TdxCertificate, TdxCollateral, TdxPckTcb,
    TdxPlatformIdentity, TdxQuote, TdxRevocationEvidence, TdxSignedCollateral,
    TdxSignedCollateralBody, TdxSignerAttestation, TdxVerifier, TdxVerifierInput,
};
use reqwest::{Url, header::HeaderMap};
use tracing::debug;
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
const ALLOWED_INTEL_HOST_SUFFIX: &str = ".trustedservices.intel.com";
const CERTIFICATE_PEM_LABEL: &str = "CERTIFICATE";

/// TDX collateral fetched from Intel PCS for one signer quote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdxCollateralFetch {
    /// Root-to-leaf PCK certificate chain carried by the quote.
    pub pck_certificate_chain: Vec<TdxCertificate>,
    /// TCB info and QE identity collateral.
    pub collateral: TdxCollateral,
    /// CRLs covering non-root certificates in the verifier input.
    pub revocation: TdxRevocationEvidence,
    /// Trusted Intel root CA hash.
    pub trusted_root_ca_hash: B256,
}

/// Hydrates TDX signer RPC attestations into prover input bytes.
#[derive(Debug)]
pub struct TdxAttestationHydrator {
    /// Intel PCS and verifier policy configuration.
    pub config: TdxAttestationConfig,
    client: reqwest::Client,
    cache: Mutex<HashMap<(B256, Vec<u8>, Vec<u8>), (u64, TdxCollateralFetch)>>,
}

impl TdxAttestationHydrator {
    /// Creates a hydrator with a hardened HTTP client.
    pub fn new(config: TdxAttestationConfig) -> Result<Self> {
        let client = config.build_http_client()?;
        Ok(Self { config, client, cache: Mutex::new(HashMap::new()) })
    }

    /// Converts a TDX signer attestation into encoded prover input.
    ///
    /// Legacy prover-input payloads are accepted only as containers for the
    /// quote, signer public key, and quote timestamp. Collateral and verifier
    /// policy are always rebuilt from registrar configuration.
    pub async fn hydrate_for_signer(
        &self,
        attestation_bytes: &[u8],
        expected_signer: Address,
    ) -> Result<Vec<u8>> {
        let attestation = Self::decode_attestation_payload(attestation_bytes)?;
        let collateral = self.fetch_collateral(&attestation.quote).await?;
        let public_key_hash = TdxVerifier::validate_public_key(&attestation.signer_public_key)
            .map_err(|e| TdxCollateralError::source(e))?;
        let actual_signer = Address::from_slice(&public_key_hash.as_slice()[12..]);
        if actual_signer != expected_signer {
            return Err(TdxCollateralError::source(format!(
                "signer mismatch: expected {expected_signer}, got {actual_signer}"
            )));
        }
        let verification_time = Self::quote_verification_time_seconds(
            attestation.quote_timestamp_millis,
            Self::now_seconds()?,
        )?;
        let verifier_input = TdxVerifierInput {
            quote: attestation.quote,
            pck_certificate_chain: collateral.pck_certificate_chain,
            collateral: collateral.collateral,
            revocation: collateral.revocation,
            trusted_root_ca_hash: collateral.trusted_root_ca_hash,
            expected_public_key: attestation.signer_public_key,
            quote_timestamp_millis: attestation.quote_timestamp_millis,
            verification_time,
            max_quote_age_seconds: self.config.max_quote_age.as_secs(),
            allowed_tcb_statuses: self.config.allowed_tcb_statuses.clone(),
        };
        Ok(TdxAttestationProverInput::new(verifier_input).encode())
    }

    /// Decodes a current signer attestation or legacy prover input payload.
    ///
    /// Legacy prover input is reduced to the fields that originate from the
    /// signer endpoint; verifier collateral and policy must be rehydrated by
    /// the registrar.
    pub fn decode_attestation_payload(attestation_bytes: &[u8]) -> Result<TdxSignerAttestation> {
        match TdxSignerAttestation::decode(attestation_bytes) {
            Ok(attestation) => Ok(attestation),
            Err(signer_attestation_error) => {
                let prover_input = TdxAttestationProverInput::decode(attestation_bytes).map_err(
                    |prover_input_error| {
                        TdxCollateralError::source(format!(
                            "failed to decode TDX attestation payload as signer attestation ({signer_attestation_error}) or legacy prover input ({prover_input_error})"
                        ))
                    },
                )?;
                let verifier_input = prover_input.verifier_input;
                Ok(TdxSignerAttestation {
                    signer_public_key: verifier_input.expected_public_key,
                    quote: verifier_input.quote,
                    quote_timestamp_millis: verifier_input.quote_timestamp_millis,
                })
            }
        }
    }

    /// Fetches Intel PCS collateral and CRLs required to verify `quote`.
    pub async fn fetch_collateral(&self, quote: &[u8]) -> Result<TdxCollateralFetch> {
        let parsed_quote = TdxQuote::parse(quote).map_err(|e| TdxCollateralError::source(e))?;
        let pck_certificate_chain = Self::pck_certificate_chain_from_quote(&parsed_quote)?;
        Self::verify_trusted_root_ca_hash(
            &pck_certificate_chain,
            self.config.trusted_root_ca_hash,
        )?;
        let pck_leaf = pck_certificate_chain
            .last()
            .ok_or_else(|| TdxCollateralError::source("PCK certificate chain is empty"))?;
        let (platform, pck_tcb) =
            TdxPlatformIdentity::platform_and_tcb_from_pck_certificate_der(&pck_leaf.raw)
                .map_err(|e| TdxCollateralError::source(e))?;

        let verification_time = Self::now_seconds()?;
        let lookup = Self::collateral_cache_lookup(&pck_certificate_chain, &platform)?;
        if let Some(fetch) = self.cached_collateral(
            &lookup,
            &parsed_quote,
            &pck_certificate_chain,
            &pck_tcb,
            verification_time,
        )? {
            return Ok(fetch);
        }

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
        let fetch = TdxCollateralFetch {
            pck_certificate_chain,
            collateral,
            revocation,
            trusted_root_ca_hash: self.config.trusted_root_ca_hash,
        };
        let expiration =
            Self::verify_collateral_for_quote(&fetch, &parsed_quote, verification_time)?;
        {
            self.cache
                .lock()
                .expect("TDX collateral cache poisoned")
                .insert(lookup, (expiration, fetch.clone()));
        }
        Ok(fetch)
    }

    /// Verifies host-side collateral and returns the earliest accepted expiration.
    pub fn verify_collateral_for_quote(
        fetch: &TdxCollateralFetch,
        parsed_quote: &ParsedTdxQuote,
        verification_time: u64,
    ) -> Result<u64> {
        let (expiration, _) = TdxVerifier::verify_quote_collateral(
            parsed_quote,
            &fetch.pck_certificate_chain,
            &fetch.collateral,
            &fetch.revocation,
            fetch.trusted_root_ca_hash,
            verification_time,
        )
        .map_err(|e| TdxCollateralError::source(e))?;
        Ok(expiration)
    }

    fn now_seconds() -> Result<u64> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| TdxCollateralError::source(e))
            .map(|duration| duration.as_secs())
    }

    /// Returns a verifier timestamp that keeps freshly collected quotes strictly in the past.
    pub fn quote_verification_time_seconds(
        quote_timestamp_millis: u64,
        now_seconds: u64,
    ) -> Result<u64> {
        let quote_timestamp_seconds = quote_timestamp_millis / 1_000;
        if quote_timestamp_seconds > now_seconds {
            return Err(TdxCollateralError::source(format!(
                "TDX quote timestamp {quote_timestamp_seconds} is in the future relative to verifier time {now_seconds}"
            )));
        }
        if quote_timestamp_seconds == now_seconds {
            return now_seconds.checked_add(1).ok_or_else(|| {
                TdxCollateralError::source("TDX quote verification timestamp overflows")
            });
        }
        Ok(now_seconds)
    }

    /// Builds the cache lookup key for a quote's platform collateral.
    pub fn collateral_cache_lookup(
        pck_certificate_chain: &[TdxCertificate],
        platform: &TdxPlatformIdentity,
    ) -> Result<(B256, Vec<u8>, Vec<u8>)> {
        let pck_issuer = pck_certificate_chain
            .iter()
            .rev()
            .nth(1)
            .or_else(|| pck_certificate_chain.first())
            .ok_or_else(|| TdxCollateralError::source("certificate chain is empty"))?
            .hash();
        Ok((pck_issuer, platform.fmspc.to_vec(), platform.pce_id.to_vec()))
    }

    /// Returns cached collateral for the given lookup, or `None` if absent.
    ///
    /// On TCB matching or verification errors the cache entry is intentionally
    /// kept: the collateral is platform-scoped and may still be valid for other
    /// quotes; the error is quote-specific, not a signal that the collateral
    /// itself is bad.
    fn cached_collateral(
        &self,
        lookup: &(B256, Vec<u8>, Vec<u8>),
        parsed_quote: &ParsedTdxQuote,
        pck_certificate_chain: &[TdxCertificate],
        pck_tcb: &TdxPckTcb,
        verification_time: u64,
    ) -> Result<Option<TdxCollateralFetch>> {
        let Some(mut fetch) =
            self.cache.lock().expect("TDX collateral cache poisoned").get(lookup).and_then(
                |(expiration, fetch)| (*expiration > verification_time).then(|| fetch.clone()),
            )
        else {
            return Ok(None);
        };

        fetch.pck_certificate_chain = pck_certificate_chain.to_vec();
        let cached_tcb_info = &fetch.collateral.tcb_info;
        if let Err(error) = cached_tcb_info
            .tcb_info_document()
            .and_then(|document| document.tcb_info.tcb_status_for_quote(parsed_quote, pck_tcb))
        {
            debug!(error = %error, "cached TDX collateral failed quote TCB matching");
            return Err(TdxCollateralError::source(error));
        }

        match Self::verify_collateral_for_quote(&fetch, parsed_quote, verification_time) {
            Ok(expiration) => {
                debug!(expiration, "using cached TDX collateral");
                Ok(Some(fetch))
            }
            Err(error) => {
                debug!(error = %error, "cached TDX collateral failed quote verification");
                Err(error)
            }
        }
    }

    fn pck_certificate_chain_from_quote(
        parsed_quote: &ParsedTdxQuote,
    ) -> Result<Vec<TdxCertificate>> {
        if parsed_quote.certification_data_type != PCK_CERT_CHAIN_CERTIFICATION_DATA_TYPE {
            return Err(TdxCollateralError::source(format!(
                "unsupported TDX quote certification data type {}",
                parsed_quote.certification_data_type,
            )));
        }
        Self::certificate_chain_from_pem(&parsed_quote.certification_data)
    }

    async fn fetch_tcb_info(&self, platform: &TdxPlatformIdentity) -> Result<TdxSignedCollateral> {
        let mut url =
            self.config.pcs_tdx_base_url.join("tcb").map_err(|e| TdxCollateralError::source(e))?;
        url.query_pairs_mut()
            .append_pair("fmspc", &hex::encode(&platform.fmspc))
            .append_pair("pceid", &hex::encode(&platform.pce_id));
        let chain_headers = [TCB_INFO_ISSUER_CHAIN_HEADER, LEGACY_TCB_INFO_ISSUER_CHAIN_HEADER];
        let signature_headers = [TCB_INFO_SIGNATURE_HEADER, LEGACY_TCB_INFO_SIGNATURE_HEADER];
        self.fetch_signed_collateral(
            url,
            &chain_headers,
            Some(&signature_headers),
            TdxSignedCollateralBody::TcbInfo,
        )
        .await
    }

    async fn fetch_qe_identity(&self) -> Result<TdxSignedCollateral> {
        let url = self
            .config
            .pcs_tdx_base_url
            .join("qe/identity")
            .map_err(|e| TdxCollateralError::source(e))?;
        let chain_headers = [QE_IDENTITY_ISSUER_CHAIN_HEADER];
        self.fetch_signed_collateral(url, &chain_headers, None, TdxSignedCollateralBody::QeIdentity)
            .await
    }

    async fn fetch_signed_collateral(
        &self,
        url: Url,
        chain_headers: &[&str],
        signature_headers: Option<&[&str]>,
        body_kind: TdxSignedCollateralBody,
    ) -> Result<TdxSignedCollateral> {
        let response = self.get(url).await?;
        let headers = response.headers().clone();
        let raw = Self::limited_body(response).await?;
        let signing_chain = Self::certificate_chain_from_header(&headers, chain_headers)?;
        Self::verify_trusted_root_ca_hash(&signing_chain, self.config.trusted_root_ca_hash)?;
        let signature = match signature_headers
            .and_then(|header_names| header_names.iter().find_map(|header| headers.get(*header)))
        {
            Some(value) => Self::signature_from_hex(
                value.to_str().map_err(|e| TdxCollateralError::source(e))?,
            )?,
            None => Self::qe_identity_signature_from_json(&raw)?,
        };
        let collateral = TdxSignedCollateral { raw, signing_chain, signature };
        collateral.signed_validity(body_kind).map_err(|e| TdxCollateralError::source(e))?;
        Ok(collateral)
    }

    async fn fetch_revocation_evidence(
        &self,
        chains: &[&[TdxCertificate]],
    ) -> Result<TdxRevocationEvidence> {
        let mut seen = HashSet::new();
        let mut urls = Vec::new();
        for chain in chains {
            for certificate in chain.iter().skip(1) {
                let crl_url = Self::crl_distribution_point(&certificate.raw)?;
                if !seen.insert(crl_url.clone()) {
                    continue;
                }
                let url = Url::parse(&crl_url).map_err(|e| TdxCollateralError::source(e))?;
                if !Self::is_allowed_intel_url(&url) {
                    return Err(TdxCollateralError::source(format!(
                        "TDX certificate CRL URL is not an allowed Intel URL: {crl_url}"
                    )));
                }
                urls.push(url);
            }
        }
        let mut certificate_crls = Vec::with_capacity(urls.len());
        for url in urls {
            let response = self.get(url).await?;
            certificate_crls.push(Self::limited_body(response).await?);
        }
        Ok(TdxRevocationEvidence { certificate_crls })
    }

    async fn get(&self, url: Url) -> Result<reqwest::Response> {
        let response =
            self.client.get(url.clone()).send().await.map_err(|e| TdxCollateralError::source(e))?;
        if !response.status().is_success() {
            let status = response.status();
            return Err(TdxCollateralError::source(format!(
                "Intel PCS request to {url} failed with status {status}"
            )));
        }
        Ok(response)
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

    fn certificate_chain_from_header(
        headers: &HeaderMap,
        header_names: &[&str],
    ) -> Result<Vec<TdxCertificate>> {
        let value = Self::header_value(headers, header_names)?;
        let decoded = Self::percent_decode(value)?;
        Self::certificate_chain_from_pem(&decoded)
    }

    fn header_value<'a>(headers: &'a HeaderMap, header_names: &[&str]) -> Result<&'a str> {
        for header in header_names {
            if let Some(value) = headers.get(*header) {
                return value.to_str().map_err(|e| TdxCollateralError::source(e));
            }
        }
        Err(TdxCollateralError::source(format!(
            "Intel PCS response missing {}",
            header_names.join(" or ")
        )))
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
        Self::signature_from_hex(value)
    }

    fn signature_from_hex(value: &str) -> Result<Bytes> {
        let trimmed = value.trim();
        let signature = trimmed.strip_prefix("0x").unwrap_or(trimmed);
        hex::decode(signature).map(Bytes::from).map_err(|e| TdxCollateralError::source(e))
    }

    fn certificate_chain_from_pem(pem_bytes: &[u8]) -> Result<Vec<TdxCertificate>> {
        let mut certs = Vec::new();
        for pem in Pem::iter_from_buffer(pem_bytes) {
            let pem =
                pem.map_err(|e| TdxCollateralError::source(format!("PEM parse failed: {e}")))?;
            if pem.label == CERTIFICATE_PEM_LABEL {
                certs.push(Bytes::from(pem.contents));
            }
        }
        Self::chain_from_der_certs(certs)
    }

    fn chain_from_der_certs(certs: Vec<Bytes>) -> Result<Vec<TdxCertificate>> {
        if certs.is_empty() {
            return Err(TdxCollateralError::source("certificate chain is empty"));
        }
        let authenticated = certs
            .iter()
            .map(|cert| TdxCertificate::authenticated_from_der(cert))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| TdxCollateralError::source(e))?;
        let ordered_indexes = Self::root_to_leaf_indexes(&authenticated)?;
        let mut ordered = Vec::with_capacity(ordered_indexes.len());
        for index in ordered_indexes {
            ordered.push(TdxCertificate { raw: certs[index].clone() });
        }
        Ok(ordered)
    }

    fn root_to_leaf_indexes(certs: &[AuthenticatedTdxCertificate]) -> Result<Vec<usize>> {
        let mut root_index = certs
            .iter()
            .position(|cert| cert.issuer_name == cert.subject_name)
            .ok_or_else(|| TdxCollateralError::source("certificate chain root is missing"))?;
        let mut ordered = Vec::with_capacity(certs.len());
        ordered.push(root_index);

        while ordered.len() < certs.len() {
            let parent = &certs[root_index];
            let Some(child_index) = certs.iter().enumerate().find_map(|(index, cert)| {
                (!ordered.contains(&index) && cert.issuer_name == parent.subject_name)
                    .then_some(index)
            }) else {
                return Err(TdxCollateralError::source("certificate chain is not contiguous"));
            };
            ordered.push(child_index);
            root_index = child_index;
        }
        Ok(ordered)
    }

    fn verify_trusted_root_ca_hash(
        chain: &[TdxCertificate],
        trusted_root_ca_hash: B256,
    ) -> Result<()> {
        let actual_root_ca_hash = chain
            .first()
            .ok_or_else(|| TdxCollateralError::source("certificate chain is empty"))?
            .hash();
        if actual_root_ca_hash != trusted_root_ca_hash {
            return Err(TdxCollateralError::source(format!(
                "TDX certificate chain root is not trusted: expected {trusted_root_ca_hash}, got {actual_root_ca_hash}"
            )));
        }
        Ok(())
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
                host == "trustedservices.intel.com" || host.ends_with(ALLOWED_INTEL_HOST_SUFFIX)
            })
    }

    fn percent_decode(value: &str) -> Result<Vec<u8>> {
        let bytes = value.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] != b'%' {
                decoded.push(bytes[index]);
                index += 1;
                continue;
            }
            let Some(hex_bytes) = bytes.get(index + 1..index + 3) else {
                return Err(TdxCollateralError::source("invalid percent-encoded Intel PCS header"));
            };
            let text = std::str::from_utf8(hex_bytes).map_err(|e| TdxCollateralError::source(e))?;
            let value = u8::from_str_radix(text, 16).map_err(|_| {
                TdxCollateralError::source("invalid percent-encoded Intel PCS header")
            })?;
            decoded.push(value);
            index += 3;
        }
        Ok(decoded)
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use reqwest::header::HeaderValue;

    use super::*;

    const MINIMAL_CERTIFICATE_DER: &[u8] = &alloy_primitives::hex!(
        "308201093081b7a003020102020101300a06082a8648ce3d04030230123110300e06035504030c0766697874757265301e170d3730303130313030303130305a170d3730303130313030313030305a30123110300e06035504030c07666978747572653059301306072a8648ce3d020106082a8648ce3d0301070342000101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101300a06082a8648ce3d04030203410001010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101"
    );

    fn certificate_with_raw(raw: &'static [u8]) -> TdxCertificate {
        TdxCertificate { raw: Bytes::from_static(raw) }
    }

    fn minimal_certificate() -> TdxCertificate {
        TdxCertificate { raw: Bytes::from_static(MINIMAL_CERTIFICATE_DER) }
    }

    fn signed_collateral(
        body_key: &'static str,
        issue_time: u64,
        next_update: u64,
    ) -> TdxSignedCollateral {
        let issue_date = unix_seconds_rfc3339(issue_time);
        let next_update_date = unix_seconds_rfc3339(next_update);
        let raw = Bytes::from(
            format!(
                r#"{{"{body_key}":{{"issueDate":"{issue_date}","nextUpdate":"{next_update_date}"}}}}"#
            )
            .into_bytes(),
        );
        TdxSignedCollateral {
            raw,
            signing_chain: vec![minimal_certificate()],
            signature: Bytes::new(),
        }
    }

    fn unix_seconds_rfc3339(timestamp: u64) -> String {
        format!("1970-01-01T00:{:02}:{:02}Z", timestamp / 60, timestamp % 60)
    }

    fn collateral_fetch(tcb_next_update: u64, qe_next_update: u64) -> TdxCollateralFetch {
        TdxCollateralFetch {
            pck_certificate_chain: vec![minimal_certificate()],
            collateral: TdxCollateral {
                tcb_info: signed_collateral("tcbInfo", 100, tcb_next_update),
                qe_identity: signed_collateral("enclaveIdentity", 100, qe_next_update),
            },
            revocation: TdxRevocationEvidence { certificate_crls: Vec::new() },
            trusted_root_ca_hash: B256::repeat_byte(0x11),
        }
    }

    fn signed_tcb_info(tdx_svn: u64) -> TdxSignedCollateral {
        let tdx_components =
            (0..16).map(|_| serde_json::json!({ "svn": tdx_svn })).collect::<Vec<_>>();
        let sgx_components = (0..16).map(|_| serde_json::json!({ "svn": 0 })).collect::<Vec<_>>();
        let raw = serde_json::json!({
            "tcbInfo": {
                "id": "TDX",
                "teeType": "0x81",
                "issueDate": "1970-01-01T00:01:40Z",
                "nextUpdate": "1970-01-01T00:05:00Z",
                "fmspc": "020202020202",
                "pceId": "0303",
                "tdxModule": {
                    "mrsigner": "00".repeat(48),
                    "attributes": "00".repeat(8),
                    "attributesMask": "00".repeat(8),
                },
                "tdxModuleIdentities": [],
                "tcbLevels": [{
                    "tcb": {
                        "pcesvn": 0,
                        "tdxtcbcomponents": tdx_components,
                        "sgxtcbcomponents": sgx_components,
                    },
                    "tcbStatus": "UpToDate",
                }],
            },
        })
        .to_string()
        .into_bytes();

        TdxSignedCollateral {
            raw: Bytes::from(raw),
            signing_chain: vec![minimal_certificate()],
            signature: Bytes::new(),
        }
    }

    fn cache_lookup() -> (B256, Vec<u8>, Vec<u8>) {
        (B256::repeat_byte(0x02), vec![0x02; 6], vec![0x03; 2])
    }

    fn parsed_quote() -> ParsedTdxQuote {
        ParsedTdxQuote {
            signed_message: Bytes::new(),
            tee_tcb_svn: [0; 16],
            mrsigner_seam: [0; 48],
            seam_attributes: [0; 8],
            mrtd: [0; 48],
            rtmr0: [0; 48],
            rtmr1: [0; 48],
            rtmr2: [0; 48],
            rtmr3: [0; 48],
            report_data: [0; 64],
            quote_signature: Bytes::new(),
            attestation_public_key: Bytes::new(),
            qe_report: Bytes::new(),
            qe_report_signature: Bytes::new(),
            qe_authentication_data: Bytes::new(),
            certification_data_type: PCK_CERT_CHAIN_CERTIFICATION_DATA_TYPE,
            certification_data: Bytes::new(),
        }
    }

    fn assert_source_contains(error: TdxCollateralError, expected: &str) {
        let source = error.source().expect("error should retain source").to_string();
        assert!(source.contains(expected), "{source}");
    }

    #[test]
    fn percent_decode_preserves_plus_and_decodes_escapes() {
        let decoded = TdxAttestationHydrator::percent_decode("a+b%0Ac").unwrap();

        assert_eq!(decoded, b"a+b\nc");
    }

    #[test]
    fn header_value_accepts_current_and_legacy_tdx_tcb_headers() {
        let header_names = [TCB_INFO_ISSUER_CHAIN_HEADER, LEGACY_TCB_INFO_ISSUER_CHAIN_HEADER];

        for (header, expected) in [
            (TCB_INFO_ISSUER_CHAIN_HEADER, "current-chain"),
            (LEGACY_TCB_INFO_ISSUER_CHAIN_HEADER, "legacy-chain"),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header, HeaderValue::from_static(expected));

            let value = TdxAttestationHydrator::header_value(&headers, &header_names).unwrap();

            assert_eq!(value, expected);
        }
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

    #[test]
    fn trusted_root_ca_hash_accepts_configured_root() {
        let root = certificate_with_raw(b"trusted-root");
        let leaf = certificate_with_raw(b"leaf");
        let trusted_root_ca_hash = root.hash();

        TdxAttestationHydrator::verify_trusted_root_ca_hash(&[root, leaf], trusted_root_ca_hash)
            .unwrap();
    }

    #[test]
    fn trusted_root_ca_hash_rejects_quote_supplied_root() {
        let untrusted_root = certificate_with_raw(b"untrusted-root");
        let leaf = certificate_with_raw(b"leaf");
        let trusted_root_ca_hash = B256::repeat_byte(0x42);

        let error = TdxAttestationHydrator::verify_trusted_root_ca_hash(
            &[untrusted_root, leaf],
            trusted_root_ca_hash,
        )
        .unwrap_err();

        assert_source_contains(error, "TDX certificate chain root is not trusted");
    }

    #[test]
    fn collateral_cache_lookup_includes_pck_issuer() {
        let pck_certificate_chain = vec![
            certificate_with_raw(b"root-a"),
            certificate_with_raw(b"pck-issuer-a"),
            certificate_with_raw(b"pck-leaf"),
        ];
        let other_pck_certificate_chain = vec![
            certificate_with_raw(b"root-b"),
            certificate_with_raw(b"pck-issuer-b"),
            certificate_with_raw(b"pck-leaf"),
        ];
        let platform = TdxPlatformIdentity {
            fmspc: Bytes::from(vec![0x02; 6]),
            pce_id: Bytes::from(vec![0x03; 2]),
        };

        let lookup =
            TdxAttestationHydrator::collateral_cache_lookup(&pck_certificate_chain, &platform)
                .unwrap();
        let other_lookup = TdxAttestationHydrator::collateral_cache_lookup(
            &other_pck_certificate_chain,
            &platform,
        )
        .unwrap();

        assert_ne!(lookup.0, other_lookup.0);
        assert_ne!(lookup, other_lookup);
    }

    #[test]
    fn cached_collateral_keeps_entry_when_tcb_info_misses_quote() {
        let hydrator = TdxAttestationHydrator::new(TdxAttestationConfig::intel_pcs()).unwrap();
        let lookup = cache_lookup();
        let mut fetch = collateral_fetch(300, 400);
        fetch.collateral.tcb_info = signed_tcb_info(1);
        hydrator
            .cache
            .lock()
            .expect("TDX collateral cache poisoned")
            .insert(lookup.clone(), (300, fetch));

        let error = hydrator
            .cached_collateral(
                &lookup,
                &parsed_quote(),
                &[minimal_certificate()],
                &TdxPckTcb { sgx_tcb_svn: [0; 16], pce_svn: 0 },
                150,
            )
            .unwrap_err();

        assert_source_contains(error, "no TCB info level matches quote TCB");
        let cache = hydrator.cache.lock().expect("TDX collateral cache poisoned");
        assert!(cache.get(&lookup).is_some());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cached_collateral_keeps_entry_when_quote_verification_fails() {
        let hydrator = TdxAttestationHydrator::new(TdxAttestationConfig::intel_pcs()).unwrap();
        let lookup = cache_lookup();
        let pck_certificate_chain = vec![minimal_certificate()];
        let mut fetch = collateral_fetch(300, 400);
        fetch.collateral.tcb_info = signed_tcb_info(0);
        fetch.trusted_root_ca_hash = pck_certificate_chain[0].hash();
        hydrator
            .cache
            .lock()
            .expect("TDX collateral cache poisoned")
            .insert(lookup.clone(), (300, fetch));

        let error = hydrator
            .cached_collateral(
                &lookup,
                &parsed_quote(),
                &pck_certificate_chain,
                &TdxPckTcb { sgx_tcb_svn: [0; 16], pce_svn: 0 },
                150,
            )
            .unwrap_err();

        assert_source_contains(error, "PCK certificate chain is invalid");
        let cache = hydrator.cache.lock().expect("TDX collateral cache poisoned");
        assert!(cache.get(&lookup).is_some());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn quote_verification_time_uses_expected_verifier_time() {
        for (quote_timestamp_millis, now_seconds, expected) in
            [(149_999, 150, 150), (150_999, 150, 151)]
        {
            let verification_time = TdxAttestationHydrator::quote_verification_time_seconds(
                quote_timestamp_millis,
                now_seconds,
            )
            .unwrap();

            assert_eq!(verification_time, expected);
        }
    }

    #[test]
    fn quote_verification_time_rejects_future_quote() {
        let error =
            TdxAttestationHydrator::quote_verification_time_seconds(151_000, 150).unwrap_err();

        assert_source_contains(error, "in the future");
    }
}
