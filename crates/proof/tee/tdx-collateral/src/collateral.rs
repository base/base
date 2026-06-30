//! TDX attestation collateral hydration for proof generation.

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{Address, B256, Bytes, hex};
use base_proof_tee_tdx_attestation_prover::TdxAttestationProverInput;
use base_proof_tee_tdx_verifier::{
    AuthenticatedTdxCertificate, CollateralVerifier, ParsedTdxQuote, TdxCertificate,
    TdxCertificateRevocationList, TdxCollateral, TdxPckTcb, TdxPlatformIdentity, TdxQuote,
    TdxQuotePolicy, TdxRevocationEvidence, TdxSignedCollateral, TdxSignedCollateralBody,
    TdxSignerAttestation, TdxVerifierError, TdxVerifierInput,
};
use reqwest::{
    StatusCode,
    header::{HeaderMap, HeaderName},
};
use tracing::debug;
use x509_parser::{
    certificate::X509Certificate,
    extensions::{DistributionPointName, GeneralName, ParsedExtension},
    pem::parse_x509_pem,
    prelude::FromDer,
};

use crate::{Result, TdxAttestationConfig, TdxCollateralError};

/// Maximum allowed Intel PCS response size.
pub const MAX_TDX_COLLATERAL_RESPONSE_BYTES: u64 = 10 * 1024 * 1024;

const PCK_CERT_CHAIN_CERTIFICATION_DATA_TYPE: u16 = 5;
const TCB_INFO_ISSUER_CHAIN_HEADER: &str = "tcb-info-issuer-chain";
const LEGACY_TCB_INFO_ISSUER_CHAIN_HEADER: &str = "sgx-tcb-info-issuer-chain";
const TCB_INFO_SIGNATURE_HEADER: &str = "tcb-info-signature";
const LEGACY_TCB_INFO_SIGNATURE_HEADER: &str = "sgx-tcb-info-signature";
const QE_IDENTITY_ISSUER_CHAIN_HEADER: &str = "sgx-enclave-identity-issuer-chain";
const QE_IDENTITY_SIGNATURE_FIELD: &str = "signature";
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

/// Cache lookup fields available before collateral is fetched.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TdxCollateralCacheLookup {
    /// Hash of the trusted issuer root certificate for this collateral family.
    pub issuer: B256,
    /// Hash of the PCK leaf issuer certificate whose CRL is required.
    pub pck_issuer: B256,
    /// Intel FMSPC bytes for the platform.
    pub fmspc: Vec<u8>,
    /// PCK CA/PCE selector bytes used for the collateral request.
    pub ca: Vec<u8>,
    /// PCS collateral API version, such as `v4`.
    pub collateral_version: String,
}

/// Full cache key for a fetched collateral bundle.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TdxCollateralCacheKey {
    /// Lookup fields shared by fresh and expired entries.
    pub lookup: TdxCollateralCacheLookup,
    /// Earliest accepted expiration across collateral, certificates, and CRLs.
    pub expiration: u64,
}

/// Cache entry for one TDX collateral bundle.
#[derive(Debug, Clone)]
pub struct TdxCollateralCacheEntry {
    /// Full key, including collateral expiration.
    pub key: TdxCollateralCacheKey,
    /// Cached collateral bundle.
    pub fetch: TdxCollateralFetch,
}

/// In-memory cache for Intel PCS collateral bundles.
#[derive(Debug, Default)]
pub struct TdxCollateralCache {
    entries: HashMap<TdxCollateralCacheKey, TdxCollateralFetch>,
    current: HashMap<TdxCollateralCacheLookup, TdxCollateralCacheKey>,
}

impl TdxCollateralCache {
    /// Returns a fresh cache entry for the lookup key, if present.
    pub fn get(
        &self,
        lookup: &TdxCollateralCacheLookup,
        now_seconds: u64,
    ) -> Option<TdxCollateralCacheEntry> {
        let key = self.current.get(lookup)?;
        if key.expiration <= now_seconds {
            return None;
        }
        self.entries
            .get(key)
            .cloned()
            .map(|fetch| TdxCollateralCacheEntry { key: key.clone(), fetch })
    }

    /// Inserts a collateral bundle and returns the full cache key.
    pub fn insert(
        &mut self,
        lookup: TdxCollateralCacheLookup,
        expiration: u64,
        fetch: TdxCollateralFetch,
    ) -> TdxCollateralCacheKey {
        let key = TdxCollateralCacheKey { lookup: lookup.clone(), expiration };
        if let Some(old_key) = self.current.insert(lookup, key.clone()) {
            self.entries.remove(&old_key);
        }
        self.entries.insert(key.clone(), fetch);
        key
    }

    /// Removes the current entry for `lookup`, if one exists.
    pub fn remove(&mut self, lookup: &TdxCollateralCacheLookup) {
        if let Some(key) = self.current.remove(lookup) {
            self.entries.remove(&key);
        }
    }

    /// Returns the earliest expiration among currently fresh cache entries.
    pub fn earliest_expiration(&self, now_seconds: u64) -> Option<u64> {
        self.current
            .values()
            .filter(|key| key.expiration > now_seconds && self.entries.contains_key(*key))
            .map(|key| key.expiration)
            .min()
    }

    /// Returns the number of cached collateral bundles.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true when the cache has no collateral bundles.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Host-side TDX collateral provider with Intel PCS retrieval and caching.
#[derive(Debug, Clone)]
pub struct TdxCollateralProvider {
    hydrator: TdxAttestationHydrator,
}

impl TdxCollateralProvider {
    /// Creates a provider with a hardened HTTP client.
    pub fn new(config: TdxAttestationConfig) -> Result<Self> {
        Ok(Self { hydrator: TdxAttestationHydrator::new(config)? })
    }

    /// Fetches and caches collateral for a TDX quote.
    pub async fn fetch_collateral(&self, quote: &[u8]) -> Result<TdxCollateralFetch> {
        self.hydrator.fetch_collateral(quote).await
    }

    /// Returns the shared in-memory collateral cache.
    pub fn cache(&self) -> Arc<Mutex<TdxCollateralCache>> {
        Arc::clone(&self.hydrator.cache)
    }
}

/// Hydrates TDX signer RPC attestations into prover input bytes.
#[derive(Debug, Clone)]
pub struct TdxAttestationHydrator {
    /// Intel PCS and verifier policy configuration.
    pub config: TdxAttestationConfig,
    client: reqwest::Client,
    cache: Arc<Mutex<TdxCollateralCache>>,
}

impl TdxAttestationHydrator {
    /// Creates a hydrator with a hardened HTTP client.
    pub fn new(config: TdxAttestationConfig) -> Result<Self> {
        let client = config.build_http_client()?;
        Ok(Self { config, client, cache: Arc::new(Mutex::new(TdxCollateralCache::default())) })
    }

    /// Returns true if `attestation_bytes` are already encoded TDX prover input.
    pub fn is_encoded_prover_input(attestation_bytes: &[u8]) -> bool {
        TdxAttestationProverInput::decode(attestation_bytes).is_ok()
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
            expected_signer,
            quote_timestamp_millis: attestation.quote_timestamp_millis,
            verification_time,
            policy: TdxQuotePolicy { max_quote_age_seconds: self.config.max_quote_age.as_secs() },
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
                        TdxCollateralError::source(Box::new(
                            TdxHydrationError::AttestationPayloadDecode {
                                signer_attestation_error: signer_attestation_error.to_string(),
                                prover_input_error: prover_input_error.to_string(),
                            },
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
        let parsed_quote =
            TdxQuote::parse(quote).map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        let pck_certificate_chain = Self::pck_certificate_chain_from_quote(&parsed_quote)?;
        Self::verify_trusted_root_ca_hash(
            &pck_certificate_chain,
            self.config.trusted_root_ca_hash,
        )?;
        let pck_leaf = pck_certificate_chain
            .last()
            .ok_or_else(|| TdxCollateralError::source("PCK certificate chain is empty"))?;
        let platform = TdxPlatformIdentity::from_pck_certificate_der(&pck_leaf.raw)
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        let pck_tcb = TdxPckTcb::from_pck_certificate_der(&pck_leaf.raw)
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;

        let verification_time = Self::now_seconds()?;
        let lookup = Self::collateral_cache_lookup(
            &pck_certificate_chain,
            &platform,
            &self.config.pcs_tdx_base_url,
        )?;
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
        let tcb_status = tcb_info
            .tcb_status_for_quote(&parsed_quote, &pck_tcb)
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        let collateral = TdxCollateral { tcb_info, qe_identity, tcb_status };
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
            self.cache_lock()?.insert(lookup, expiration, fetch.clone());
        }
        Ok(fetch)
    }

    fn cache_poisoned_error(error: String) -> TdxCollateralError {
        TdxCollateralError::source(Box::new(TdxHydrationError::CachePoisoned { error }))
    }

    fn cache_lock(&self) -> Result<std::sync::MutexGuard<'_, TdxCollateralCache>> {
        self.cache.lock().map_err(|e| Self::cache_poisoned_error(e.to_string()))
    }

    /// Verifies host-side collateral and returns the earliest accepted expiration.
    pub fn verify_collateral_for_quote(
        fetch: &TdxCollateralFetch,
        parsed_quote: &ParsedTdxQuote,
        verification_time: u64,
    ) -> Result<u64> {
        let pck_leaf_key = CollateralVerifier::verify_certificate_chain(
            &fetch.pck_certificate_chain,
            fetch.trusted_root_ca_hash,
            verification_time,
            &fetch.revocation,
        )
        .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        TdxQuote::verify_qe_report(parsed_quote, &pck_leaf_key)
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        TdxQuote::verify_signature(parsed_quote)
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;

        for (collateral, body_kind, error_mapper) in [
            (
                &fetch.collateral.tcb_info,
                TdxSignedCollateralBody::TcbInfo,
                TdxVerifierError::TcbInfoInvalid as fn(String) -> TdxVerifierError,
            ),
            (
                &fetch.collateral.qe_identity,
                TdxSignedCollateralBody::QeIdentity,
                TdxVerifierError::QeIdentityInvalid,
            ),
        ] {
            CollateralVerifier::verify_signed_collateral(
                collateral,
                body_kind,
                fetch.trusted_root_ca_hash,
                verification_time,
                &fetch.revocation,
                error_mapper,
            )
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        }

        let pck_leaf = fetch.pck_certificate_chain.last().ok_or_else(|| {
            TdxCollateralError::source(Box::new(TdxVerifierError::PckCertChainInvalid(
                "certificate chain is empty".into(),
            )))
        })?;
        let pck_platform = TdxPlatformIdentity::from_pck_certificate_der(&pck_leaf.raw)
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        let pck_tcb = TdxPckTcb::from_pck_certificate_der(&pck_leaf.raw)
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        let tcb_info_document = fetch
            .collateral
            .tcb_info
            .tcb_info_document()
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        tcb_info_document
            .tcb_info
            .verify_platform(&pck_platform)
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        let qe_identity_document = fetch
            .collateral
            .qe_identity
            .qe_identity_document()
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        qe_identity_document
            .enclave_identity
            .verify_qe_report(parsed_quote)
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        let tcb_status = tcb_info_document
            .tcb_info
            .tcb_status_for_quote(parsed_quote, &pck_tcb)
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        if tcb_status != fetch.collateral.tcb_status {
            return Err(TdxCollateralError::source(Box::new(TdxVerifierError::TcbInfoInvalid(
                "collateral TCB status does not match quote".into(),
            ))));
        }

        Self::validate_collateral_freshness(fetch, verification_time)
    }

    fn now_seconds() -> Result<u64> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| TdxCollateralError::source(Box::new(e)))
            .map(|duration| duration.as_secs())
    }

    /// Returns a verifier timestamp that keeps freshly collected quotes strictly in the past.
    pub fn quote_verification_time_seconds(
        quote_timestamp_millis: u64,
        now_seconds: u64,
    ) -> Result<u64> {
        let quote_timestamp_seconds = quote_timestamp_millis / 1_000;
        if quote_timestamp_seconds > now_seconds {
            return Err(TdxCollateralError::source(Box::new(
                TdxHydrationError::FutureQuoteTimestamp { quote_timestamp_seconds, now_seconds },
            )));
        }
        if quote_timestamp_seconds == now_seconds {
            return now_seconds.checked_add(1).ok_or_else(|| {
                TdxCollateralError::source(Box::new(TdxHydrationError::TimestampOverflow))
            });
        }
        Ok(now_seconds)
    }

    /// Builds the cache lookup key for a quote's platform collateral.
    pub fn collateral_cache_lookup(
        pck_certificate_chain: &[TdxCertificate],
        platform: &TdxPlatformIdentity,
        pcs_tdx_base_url: &url::Url,
    ) -> Result<TdxCollateralCacheLookup> {
        let issuer = pck_certificate_chain
            .first()
            .ok_or_else(|| TdxCollateralError::source("certificate chain is empty"))?
            .hash();
        let pck_issuer = pck_certificate_chain
            .iter()
            .rev()
            .nth(1)
            .unwrap_or_else(|| {
                pck_certificate_chain
                    .first()
                    .expect("certificate chain is non-empty after issuer lookup")
            })
            .hash();
        Ok(TdxCollateralCacheLookup {
            issuer,
            pck_issuer,
            fmspc: platform.fmspc.to_vec(),
            ca: platform.pce_id.to_vec(),
            collateral_version: Self::collateral_version(pcs_tdx_base_url),
        })
    }

    /// Extracts the PCS collateral version from a base URL.
    pub fn collateral_version(pcs_tdx_base_url: &url::Url) -> String {
        pcs_tdx_base_url
            .path_segments()
            .into_iter()
            .flatten()
            .rev()
            .find(|segment| segment.starts_with('v'))
            .unwrap_or("unknown")
            .to_string()
    }

    /// Validates host-side collateral freshness and returns the earliest expiration.
    pub fn validate_collateral_freshness(
        fetch: &TdxCollateralFetch,
        verification_time: u64,
    ) -> Result<u64> {
        let mut expiration = u64::MAX;
        for certificate in fetch
            .pck_certificate_chain
            .iter()
            .chain(fetch.collateral.tcb_info.signing_chain.iter())
            .chain(fetch.collateral.qe_identity.signing_chain.iter())
        {
            let certificate = TdxCertificate::authenticated_from_der(&certificate.raw)
                .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
            if verification_time < certificate.not_before
                || verification_time >= certificate.not_after
            {
                return Err(TdxCollateralError::source(Box::new(
                    TdxVerifierError::CollateralExpired,
                )));
            }
            expiration = expiration.min(certificate.not_after);
        }

        for (collateral, body_kind) in [
            (&fetch.collateral.tcb_info, TdxSignedCollateralBody::TcbInfo),
            (&fetch.collateral.qe_identity, TdxSignedCollateralBody::QeIdentity),
        ] {
            expiration = expiration.min(Self::validate_signed_collateral_freshness(
                collateral,
                body_kind,
                verification_time,
            )?);
        }

        for chain in [
            fetch.pck_certificate_chain.as_slice(),
            fetch.collateral.tcb_info.signing_chain.as_slice(),
            fetch.collateral.qe_identity.signing_chain.as_slice(),
        ] {
            if chain.len() <= 1 {
                continue;
            }
            let crl_expiration = fetch
                .revocation
                .certificate_chain_next_update(chain, verification_time)
                .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
            expiration = expiration.min(crl_expiration);
        }

        Ok(expiration)
    }

    /// Validates one signed collateral document's freshness.
    pub fn validate_signed_collateral_freshness(
        collateral: &TdxSignedCollateral,
        body_kind: TdxSignedCollateralBody,
        verification_time: u64,
    ) -> Result<u64> {
        let error_mapper = match body_kind {
            TdxSignedCollateralBody::TcbInfo => TdxVerifierError::TcbInfoInvalid,
            TdxSignedCollateralBody::QeIdentity => TdxVerifierError::QeIdentityInvalid,
        };
        let validity = collateral
            .signed_validity(body_kind, error_mapper)
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        if collateral.issue_time != validity.issue_time
            || collateral.next_update != validity.next_update
        {
            return Err(TdxCollateralError::source(Box::new(error_mapper(
                "explicit collateral validity does not match signed JSON".into(),
            ))));
        }
        if verification_time < validity.issue_time || verification_time >= validity.next_update {
            return Err(TdxCollateralError::source(Box::new(TdxVerifierError::CollateralExpired)));
        }
        Ok(validity.next_update)
    }

    /// Returns cached collateral for the given lookup, or `None` if absent.
    ///
    /// On TCB matching or verification errors the cache entry is intentionally
    /// kept: the collateral is platform-scoped and may still be valid for other
    /// quotes; the error is quote-specific, not a signal that the collateral
    /// itself is bad.
    fn cached_collateral(
        &self,
        lookup: &TdxCollateralCacheLookup,
        parsed_quote: &ParsedTdxQuote,
        pck_certificate_chain: &[TdxCertificate],
        pck_tcb: &TdxPckTcb,
        verification_time: u64,
    ) -> Result<Option<TdxCollateralFetch>> {
        let Some(entry) = self.cache_lock()?.get(lookup, verification_time) else {
            return Ok(None);
        };

        let mut fetch = entry.fetch;
        fetch.pck_certificate_chain = pck_certificate_chain.to_vec();
        let cached_tcb_info = &fetch.collateral.tcb_info;
        let tcb_status = match cached_tcb_info.tcb_status_for_quote(parsed_quote, pck_tcb) {
            Ok(tcb_status) => tcb_status,
            Err(error) => {
                debug!(error = %error, "cached TDX collateral failed quote TCB matching");
                return Err(TdxCollateralError::source(Box::new(error)));
            }
        };
        fetch.collateral.tcb_status = tcb_status;

        match Self::verify_collateral_for_quote(&fetch, parsed_quote, verification_time) {
            Ok(expiration) => {
                debug!(
                    expiration,
                    cache_key_expiration = entry.key.expiration,
                    "using cached TDX collateral"
                );
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
            return Err(TdxCollateralError::source(Box::new(
                TdxHydrationError::UnsupportedCertificationData {
                    actual: parsed_quote.certification_data_type,
                },
            )));
        }
        Self::certificate_chain_from_pem(&parsed_quote.certification_data)
    }

    async fn fetch_tcb_info(&self, platform: &TdxPlatformIdentity) -> Result<TdxSignedCollateral> {
        let mut url = self
            .config
            .pcs_tdx_base_url
            .join("tcb")
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        url.query_pairs_mut()
            .append_pair("fmspc", &hex::encode(&platform.fmspc))
            .append_pair("pceid", &hex::encode(&platform.pce_id));
        let chain_headers = [
            HeaderName::from_static(TCB_INFO_ISSUER_CHAIN_HEADER),
            HeaderName::from_static(LEGACY_TCB_INFO_ISSUER_CHAIN_HEADER),
        ];
        let signature_headers = [
            HeaderName::from_static(TCB_INFO_SIGNATURE_HEADER),
            HeaderName::from_static(LEGACY_TCB_INFO_SIGNATURE_HEADER),
        ];
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
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        let chain_headers = [HeaderName::from_static(QE_IDENTITY_ISSUER_CHAIN_HEADER)];
        self.fetch_signed_collateral(url, &chain_headers, None, TdxSignedCollateralBody::QeIdentity)
            .await
    }

    async fn fetch_signed_collateral(
        &self,
        url: url::Url,
        chain_headers: &[HeaderName],
        signature_headers: Option<&[HeaderName]>,
        body_kind: TdxSignedCollateralBody,
    ) -> Result<TdxSignedCollateral> {
        let response = self.get(url).await?;
        let headers = response.headers().clone();
        let raw = Self::limited_body(response).await?;
        let signing_chain = Self::certificate_chain_from_header(&headers, chain_headers)?;
        Self::verify_trusted_root_ca_hash(&signing_chain, self.config.trusted_root_ca_hash)?;
        let signature = match signature_headers {
            Some(signature_header_names) => Self::signature_from_header_or_json_field(
                &headers,
                signature_header_names,
                &raw,
                QE_IDENTITY_SIGNATURE_FIELD,
            )?,
            None => Self::signature_from_json_field(&raw, QE_IDENTITY_SIGNATURE_FIELD)?,
        };
        let collateral =
            TdxSignedCollateral { raw, signing_chain, signature, issue_time: 0, next_update: 0 };
        let error_mapper = match body_kind {
            TdxSignedCollateralBody::TcbInfo => {
                TdxVerifierError::TcbInfoInvalid as fn(String) -> TdxVerifierError
            }
            TdxSignedCollateralBody::QeIdentity => TdxVerifierError::QeIdentityInvalid,
        };
        let validity = collateral
            .signed_validity(body_kind, error_mapper)
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        Ok(TdxSignedCollateral {
            issue_time: validity.issue_time,
            next_update: validity.next_update,
            ..collateral
        })
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
                let url = url::Url::parse(&crl_url)
                    .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
                if !Self::is_allowed_intel_url(&url) {
                    return Err(TdxCollateralError::source(Box::new(
                        TdxHydrationError::DisallowedCrlHost { url: crl_url },
                    )));
                }
                urls.push(url);
            }
        }
        let certificate_crls = futures::future::try_join_all(urls.into_iter().map(|url| async {
            let response = self.get(url).await?;
            let raw = Self::limited_body(response).await?;
            Ok::<_, TdxCollateralError>(TdxCertificateRevocationList { raw })
        }))
        .await?;
        Ok(TdxRevocationEvidence { certificate_crls })
    }

    async fn get(&self, url: url::Url) -> Result<reqwest::Response> {
        let response = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        if !response.status().is_success() {
            return Err(TdxCollateralError::source(Box::new(TdxHydrationError::HttpStatus {
                url: url.to_string(),
                status: response.status(),
            })));
        }
        Ok(response)
    }

    async fn limited_body(response: reqwest::Response) -> Result<Bytes> {
        if response.content_length().is_some_and(|len| len > MAX_TDX_COLLATERAL_RESPONSE_BYTES) {
            return Err(TdxCollateralError::source(Box::new(TdxHydrationError::ResponseTooLarge)));
        }
        let bytes = response.bytes().await.map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_TDX_COLLATERAL_RESPONSE_BYTES {
            return Err(TdxCollateralError::source(Box::new(TdxHydrationError::ResponseTooLarge)));
        }
        Ok(Bytes(bytes))
    }

    fn certificate_chain_from_header(
        headers: &HeaderMap,
        header_names: &[HeaderName],
    ) -> Result<Vec<TdxCertificate>> {
        let value = Self::header_value(headers, header_names)?;
        let decoded = Self::percent_decode(value)?;
        Self::certificate_chain_from_pem(&decoded)
    }

    fn header_value<'a>(headers: &'a HeaderMap, header_names: &[HeaderName]) -> Result<&'a str> {
        for header in header_names {
            if let Some(value) = headers.get(header) {
                return value.to_str().map_err(|e| TdxCollateralError::source(Box::new(e)));
            }
        }
        let header = header_names.iter().map(HeaderName::as_str).collect::<Vec<_>>().join(" or ");
        Err(TdxCollateralError::source(Box::new(TdxHydrationError::MissingHeader { header })))
    }

    fn signature_from_header(headers: &HeaderMap, header: &HeaderName) -> Result<Bytes> {
        let value = headers
            .get(header)
            .ok_or_else(|| {
                TdxCollateralError::source(Box::new(TdxHydrationError::MissingHeader {
                    header: header.as_str().to_string(),
                }))
            })?
            .to_str()
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        Self::signature_from_hex(value)
    }

    fn signature_from_header_or_json_field(
        headers: &HeaderMap,
        header_names: &[HeaderName],
        raw: &[u8],
        field: &'static str,
    ) -> Result<Bytes> {
        for header in header_names {
            if headers.contains_key(header) {
                return Self::signature_from_header(headers, header);
            }
        }
        Self::signature_from_json_field(raw, field)
    }

    fn signature_from_json_field(raw: &[u8], field: &'static str) -> Result<Bytes> {
        let document: serde_json::Value =
            serde_json::from_slice(raw).map_err(|e| TdxCollateralError::source(Box::new(e)))?;
        let value = document
            .get(field)
            .ok_or_else(|| {
                TdxCollateralError::source(Box::new(TdxHydrationError::MissingJsonField { field }))
            })?
            .as_str()
            .ok_or_else(|| {
                TdxCollateralError::source(Box::new(TdxHydrationError::InvalidJsonField { field }))
            })?;
        Self::signature_from_hex(value)
    }

    fn signature_from_hex(value: &str) -> Result<Bytes> {
        let trimmed = value.trim();
        let signature = trimmed.strip_prefix("0x").unwrap_or(trimmed);
        hex::decode(signature).map(Bytes::from).map_err(|e| TdxCollateralError::source(Box::new(e)))
    }

    fn certificate_chain_from_pem(pem_bytes: &[u8]) -> Result<Vec<TdxCertificate>> {
        let mut remaining = pem_bytes;
        let mut certs = Vec::new();
        while !remaining.iter().all(u8::is_ascii_whitespace) {
            let (rest, pem) = parse_x509_pem(remaining).map_err(|e| {
                TdxCollateralError::source(Box::new(TdxHydrationError::Pem(e.to_string())))
            })?;
            if pem.label == CERTIFICATE_PEM_LABEL {
                certs.push(Bytes::from(pem.contents));
            }
            remaining = rest;
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
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
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
        let mut used = HashSet::new();
        ordered.push(root_index);
        used.insert(root_index);

        while ordered.len() < certs.len() {
            let parent = &certs[root_index];
            let Some(child_index) = certs.iter().enumerate().find_map(|(index, cert)| {
                (!used.contains(&index) && cert.issuer_name == parent.subject_name).then_some(index)
            }) else {
                return Err(TdxCollateralError::source("certificate chain is not contiguous"));
            };
            ordered.push(child_index);
            used.insert(child_index);
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
            return Err(TdxCollateralError::source(Box::new(
                TdxHydrationError::RootCaNotTrusted {
                    expected: trusted_root_ca_hash,
                    actual: actual_root_ca_hash,
                },
            )));
        }
        Ok(())
    }

    fn crl_distribution_point(certificate_der: &[u8]) -> Result<String> {
        let (_, certificate) = X509Certificate::from_der(certificate_der)
            .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
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

    fn is_allowed_intel_url(url: &url::Url) -> bool {
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
                return Err(TdxCollateralError::source(Box::new(
                    TdxHydrationError::InvalidPercentEncoding,
                )));
            };
            let text = std::str::from_utf8(hex_bytes)
                .map_err(|e| TdxCollateralError::source(Box::new(e)))?;
            let value = u8::from_str_radix(text, 16).map_err(|_| {
                TdxCollateralError::source(Box::new(TdxHydrationError::InvalidPercentEncoding))
            })?;
            decoded.push(value);
            index += 3;
        }
        Ok(decoded)
    }
}

#[derive(Debug)]
enum TdxHydrationError {
    AttestationPayloadDecode { signer_attestation_error: String, prover_input_error: String },
    UnsupportedCertificationData { actual: u16 },
    MissingHeader { header: String },
    MissingJsonField { field: &'static str },
    InvalidJsonField { field: &'static str },
    HttpStatus { url: String, status: StatusCode },
    ResponseTooLarge,
    DisallowedCrlHost { url: String },
    RootCaNotTrusted { expected: B256, actual: B256 },
    FutureQuoteTimestamp { quote_timestamp_seconds: u64, now_seconds: u64 },
    TimestampOverflow,
    InvalidPercentEncoding,
    CachePoisoned { error: String },
    Pem(String),
}

impl fmt::Display for TdxHydrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AttestationPayloadDecode { signer_attestation_error, prover_input_error } => {
                write!(
                    f,
                    "failed to decode TDX attestation payload as signer attestation ({signer_attestation_error}) or legacy prover input ({prover_input_error})"
                )
            }
            Self::UnsupportedCertificationData { actual } => {
                write!(f, "unsupported TDX quote certification data type {actual}")
            }
            Self::MissingHeader { header } => write!(f, "Intel PCS response missing {header}"),
            Self::MissingJsonField { field } => {
                write!(f, "Intel PCS response missing JSON field {field}")
            }
            Self::InvalidJsonField { field } => {
                write!(f, "Intel PCS response JSON field {field} is not a string")
            }
            Self::HttpStatus { url, status } => {
                write!(f, "Intel PCS request to {url} failed with status {status}")
            }
            Self::ResponseTooLarge => write!(f, "Intel PCS response exceeds size limit"),
            Self::DisallowedCrlHost { url } => {
                write!(f, "TDX certificate CRL URL is not an allowed Intel URL: {url}")
            }
            Self::RootCaNotTrusted { expected, actual } => {
                write!(
                    f,
                    "TDX certificate chain root is not trusted: expected {expected}, got {actual}"
                )
            }
            Self::FutureQuoteTimestamp { quote_timestamp_seconds, now_seconds } => {
                write!(
                    f,
                    "TDX quote timestamp {quote_timestamp_seconds} is in the future relative to verifier time {now_seconds}"
                )
            }
            Self::TimestampOverflow => write!(f, "TDX quote verification timestamp overflows"),
            Self::InvalidPercentEncoding => write!(f, "invalid percent-encoded Intel PCS header"),
            Self::CachePoisoned { error } => {
                write!(f, "TDX collateral cache lock poisoned: {error}")
            }
            Self::Pem(error) => write!(f, "PEM parse failed: {error}"),
        }
    }
}

impl Error for TdxHydrationError {}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;

    use super::*;

    fn certificate_with_raw(raw: &'static [u8]) -> TdxCertificate {
        TdxCertificate { raw: Bytes::from_static(raw) }
    }

    fn certificate_with_validity(_not_before: u64, _not_after: u64) -> TdxCertificate {
        TdxCertificate { raw: Bytes::from(minimal_certificate_der()) }
    }

    fn minimal_certificate_der() -> Vec<u8> {
        let key = [1; 65];
        let tbs = der_sequence(&[
            der_tag(0xa0, &der_integer(&[2])),
            der_integer(&[1]),
            x509_algorithm_identifier(),
            x509_name(),
            der_sequence(&[der_utc_time("700101000100Z"), der_utc_time("700101001000Z")]),
            x509_name(),
            x509_subject_public_key_info(&key),
        ]);
        der_sequence(&[tbs, x509_algorithm_identifier(), der_bit_string(&[1; 64])])
    }

    fn der_len(len: usize) -> Vec<u8> {
        if len < 128 {
            return vec![len as u8];
        }

        let bytes = len.to_be_bytes();
        let first_non_zero = bytes.iter().position(|byte| *byte != 0).unwrap_or(bytes.len() - 1);
        let significant = &bytes[first_non_zero..];
        let mut out = vec![0x80 | significant.len() as u8];
        out.extend_from_slice(significant);
        out
    }

    fn der_tag(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend_from_slice(&der_len(content.len()));
        out.extend_from_slice(content);
        out
    }

    fn der_sequence(parts: &[Vec<u8>]) -> Vec<u8> {
        der_tag(0x30, &parts.concat())
    }

    fn der_set(parts: &[Vec<u8>]) -> Vec<u8> {
        der_tag(0x31, &parts.concat())
    }

    fn der_integer(bytes: &[u8]) -> Vec<u8> {
        der_tag(0x02, bytes)
    }

    fn der_bit_string(bytes: &[u8]) -> Vec<u8> {
        let mut value = vec![0];
        value.extend_from_slice(bytes);
        der_tag(0x03, &value)
    }

    fn der_utc_time(value: &str) -> Vec<u8> {
        der_tag(0x17, value.as_bytes())
    }

    fn x509_name() -> Vec<u8> {
        der_sequence(&[der_set(&[der_sequence(&[
            vec![0x06, 0x03, 0x55, 0x04, 0x03],
            der_tag(0x0c, b"fixture"),
        ])])])
    }

    fn x509_algorithm_identifier() -> Vec<u8> {
        der_sequence(&[vec![0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02]])
    }

    fn x509_subject_public_key_info(key: &[u8]) -> Vec<u8> {
        der_sequence(&[
            der_sequence(&[
                vec![0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01],
                vec![0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07],
            ]),
            der_bit_string(key),
        ])
    }

    fn signed_collateral(
        body_key: &'static str,
        issue_time: u64,
        next_update: u64,
    ) -> TdxSignedCollateral {
        let raw = Bytes::from(
            format!(r#"{{"{body_key}":{{"issueDate":{issue_time},"nextUpdate":{next_update}}}}}"#)
                .into_bytes(),
        );
        TdxSignedCollateral {
            raw,
            signing_chain: vec![certificate_with_validity(issue_time, next_update + 100)],
            signature: Bytes::new(),
            issue_time,
            next_update,
        }
    }

    fn collateral_fetch(tcb_next_update: u64, qe_next_update: u64) -> TdxCollateralFetch {
        TdxCollateralFetch {
            pck_certificate_chain: vec![certificate_with_validity(100, 500)],
            collateral: TdxCollateral {
                tcb_info: signed_collateral("tcbInfo", 100, tcb_next_update),
                qe_identity: signed_collateral("enclaveIdentity", 100, qe_next_update),
                tcb_status: base_proof_tee_tdx_verifier::IntelTcbStatus::UpToDate,
            },
            revocation: TdxRevocationEvidence { certificate_crls: Vec::new() },
            trusted_root_ca_hash: B256::repeat_byte(0x11),
        }
    }

    fn signed_tcb_info(tdx_svn: u64, issue_time: u64, next_update: u64) -> TdxSignedCollateral {
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
            signing_chain: vec![certificate_with_validity(issue_time, next_update + 100)],
            signature: Bytes::new(),
            issue_time,
            next_update,
        }
    }

    fn cache_lookup() -> TdxCollateralCacheLookup {
        TdxCollateralCacheLookup {
            issuer: B256::repeat_byte(0x01),
            pck_issuer: B256::repeat_byte(0x02),
            fmspc: vec![0x02; 6],
            ca: vec![0x03; 2],
            collateral_version: "v4".to_string(),
        }
    }

    fn parsed_quote() -> ParsedTdxQuote {
        ParsedTdxQuote {
            header: base_proof_tee_tdx_verifier::TdxQuoteHeader {
                version: 4,
                attestation_key_type: 2,
                tee_type: 0x81,
                reserved: [0; 4],
            },
            header_bytes: Bytes::new(),
            report_body: Bytes::new(),
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

    #[test]
    fn percent_decode_preserves_plus_and_decodes_escapes() {
        let decoded = TdxAttestationHydrator::percent_decode("a+b%0Ac").unwrap();

        assert_eq!(decoded, b"a+b\nc");
    }

    #[test]
    fn header_value_accepts_current_tdx_tcb_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static(TCB_INFO_ISSUER_CHAIN_HEADER),
            HeaderValue::from_static("current-chain"),
        );
        let header_names = [
            HeaderName::from_static(TCB_INFO_ISSUER_CHAIN_HEADER),
            HeaderName::from_static(LEGACY_TCB_INFO_ISSUER_CHAIN_HEADER),
        ];

        let value = TdxAttestationHydrator::header_value(&headers, &header_names).unwrap();

        assert_eq!(value, "current-chain");
    }

    #[test]
    fn header_value_accepts_legacy_sgx_tcb_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static(LEGACY_TCB_INFO_ISSUER_CHAIN_HEADER),
            HeaderValue::from_static("legacy-chain"),
        );
        let header_names = [
            HeaderName::from_static(TCB_INFO_ISSUER_CHAIN_HEADER),
            HeaderName::from_static(LEGACY_TCB_INFO_ISSUER_CHAIN_HEADER),
        ];

        let value = TdxAttestationHydrator::header_value(&headers, &header_names).unwrap();

        assert_eq!(value, "legacy-chain");
    }

    #[test]
    fn qe_identity_signature_from_json_body_decodes_top_level_signature() {
        let raw = br#"{"enclaveIdentity":{},"signature":"0x0102ff"}"#;

        let signature =
            TdxAttestationHydrator::signature_from_json_field(raw, QE_IDENTITY_SIGNATURE_FIELD)
                .unwrap();

        assert_eq!(signature, Bytes::from_static(&[0x01, 0x02, 0xff]));
    }

    #[test]
    fn tcb_info_signature_from_json_body_decodes_top_level_signature() {
        let headers = HeaderMap::new();
        let header_names = [
            HeaderName::from_static(TCB_INFO_SIGNATURE_HEADER),
            HeaderName::from_static(LEGACY_TCB_INFO_SIGNATURE_HEADER),
        ];
        let raw = br#"{"tcbInfo":{},"signature":"0102ff"}"#;

        let signature = TdxAttestationHydrator::signature_from_header_or_json_field(
            &headers,
            &header_names,
            raw,
            QE_IDENTITY_SIGNATURE_FIELD,
        )
        .unwrap();

        assert_eq!(signature, Bytes::from_static(&[0x01, 0x02, 0xff]));
    }

    #[test]
    fn tcb_info_signature_prefers_header_when_present() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static(LEGACY_TCB_INFO_SIGNATURE_HEADER),
            HeaderValue::from_static("0x03"),
        );
        let header_names = [
            HeaderName::from_static(TCB_INFO_SIGNATURE_HEADER),
            HeaderName::from_static(LEGACY_TCB_INFO_SIGNATURE_HEADER),
        ];
        let raw = br#"{"tcbInfo":{},"signature":"0102ff"}"#;

        let signature = TdxAttestationHydrator::signature_from_header_or_json_field(
            &headers,
            &header_names,
            raw,
            QE_IDENTITY_SIGNATURE_FIELD,
        )
        .unwrap();

        assert_eq!(signature, Bytes::from_static(&[0x03]));
    }

    #[test]
    fn qe_identity_signature_from_json_body_requires_signature_field() {
        let raw = br#"{"enclaveIdentity":{}}"#;

        let error =
            TdxAttestationHydrator::signature_from_json_field(raw, QE_IDENTITY_SIGNATURE_FIELD)
                .unwrap_err();

        assert!(
            error
                .source()
                .expect("missing JSON field error should be retained as the source")
                .to_string()
                .contains("Intel PCS response missing JSON field signature")
        );
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

        assert!(
            error
                .source()
                .expect("root CA error should be retained as the source")
                .to_string()
                .contains("TDX certificate chain root is not trusted")
        );
    }

    #[test]
    fn collateral_cache_returns_fresh_hits() {
        let mut cache = TdxCollateralCache::default();
        let lookup = cache_lookup();
        let fetch = collateral_fetch(300, 400);

        let key = cache.insert(lookup.clone(), 300, fetch.clone());
        let entry = cache.get(&lookup, 200).unwrap();

        assert_eq!(entry.key, key);
        assert_eq!(entry.fetch, fetch);
    }

    #[test]
    fn collateral_cache_misses_unknown_or_expired_entries() {
        let mut cache = TdxCollateralCache::default();
        let lookup = cache_lookup();
        let missing_lookup =
            TdxCollateralCacheLookup { issuer: B256::repeat_byte(0xff), ..lookup.clone() };
        cache.insert(lookup.clone(), 300, collateral_fetch(300, 400));

        assert!(cache.get(&missing_lookup, 200).is_none());
        assert!(cache.get(&lookup, 300).is_none());
    }

    #[test]
    fn collateral_cache_lookup_includes_pck_issuer() {
        let root = certificate_with_raw(b"root");
        let pck_certificate_chain = vec![
            root.clone(),
            certificate_with_raw(b"pck-issuer-a"),
            certificate_with_raw(b"pck-leaf"),
        ];
        let other_pck_certificate_chain =
            vec![root, certificate_with_raw(b"pck-issuer-b"), certificate_with_raw(b"pck-leaf")];
        let platform = TdxPlatformIdentity {
            fmspc: Bytes::from(vec![0x02; 6]),
            pce_id: Bytes::from(vec![0x03; 2]),
        };
        let pcs_tdx_base_url =
            url::Url::parse("https://api.trustedservices.intel.com/tdx/certification/v4/").unwrap();

        let lookup = TdxAttestationHydrator::collateral_cache_lookup(
            &pck_certificate_chain,
            &platform,
            &pcs_tdx_base_url,
        )
        .unwrap();
        let other_lookup = TdxAttestationHydrator::collateral_cache_lookup(
            &other_pck_certificate_chain,
            &platform,
            &pcs_tdx_base_url,
        )
        .unwrap();

        assert_eq!(lookup.issuer, other_lookup.issuer);
        assert_ne!(lookup.pck_issuer, other_lookup.pck_issuer);
        assert_ne!(lookup, other_lookup);

        let mut cache = TdxCollateralCache::default();
        cache.insert(lookup, 300, collateral_fetch(300, 400));

        assert!(cache.get(&other_lookup, 200).is_none());
    }

    #[test]
    fn collateral_cache_earliest_expiration_reports_minimum_fresh_entry() {
        let mut cache = TdxCollateralCache::default();
        let early_lookup = cache_lookup();
        let late_lookup = TdxCollateralCacheLookup { fmspc: vec![0x04; 6], ..early_lookup.clone() };

        cache.insert(early_lookup, 300, collateral_fetch(300, 400));
        cache.insert(late_lookup, 1000, collateral_fetch(1000, 1100));

        assert_eq!(cache.earliest_expiration(200), Some(300));
        assert_eq!(cache.earliest_expiration(300), Some(1000));
        assert_eq!(cache.earliest_expiration(1000), None);
    }

    #[test]
    fn cached_collateral_keeps_entry_when_tcb_info_misses_quote() {
        let hydrator = TdxAttestationHydrator::new(TdxAttestationConfig::intel_pcs()).unwrap();
        let lookup = cache_lookup();
        let mut fetch = collateral_fetch(300, 400);
        fetch.collateral.tcb_info = signed_tcb_info(1, 100, 300);
        hydrator.cache_lock().unwrap().insert(lookup.clone(), 300, fetch);

        let error = hydrator
            .cached_collateral(
                &lookup,
                &parsed_quote(),
                &[certificate_with_validity(100, 500)],
                &TdxPckTcb { sgx_tcb_svn: [0; 16], pce_svn: 0 },
                150,
            )
            .unwrap_err();

        assert!(
            error
                .source()
                .expect("TCB matching error should be retained as the source")
                .to_string()
                .contains("no TCB info level matches quote TCB")
        );
        assert!(hydrator.cache_lock().unwrap().get(&lookup, 150).is_some());
        assert_eq!(hydrator.cache_lock().unwrap().len(), 1);
    }

    #[test]
    fn cached_collateral_keeps_entry_when_quote_verification_fails() {
        let hydrator = TdxAttestationHydrator::new(TdxAttestationConfig::intel_pcs()).unwrap();
        let lookup = cache_lookup();
        let pck_certificate_chain = vec![certificate_with_validity(100, 500)];
        let mut fetch = collateral_fetch(300, 400);
        fetch.collateral.tcb_info = signed_tcb_info(0, 100, 300);
        fetch.trusted_root_ca_hash = pck_certificate_chain[0].hash();
        hydrator.cache_lock().unwrap().insert(lookup.clone(), 300, fetch);

        let error = hydrator
            .cached_collateral(
                &lookup,
                &parsed_quote(),
                &pck_certificate_chain,
                &TdxPckTcb { sgx_tcb_svn: [0; 16], pce_svn: 0 },
                150,
            )
            .unwrap_err();

        assert!(
            error
                .source()
                .expect("quote verification error should be retained as the source")
                .to_string()
                .contains("PCK certificate chain is invalid")
        );
        assert!(hydrator.cache_lock().unwrap().get(&lookup, 150).is_some());
        assert_eq!(hydrator.cache_lock().unwrap().len(), 1);
    }

    #[test]
    fn collateral_freshness_returns_earliest_expiration() {
        let fetch = collateral_fetch(300, 250);

        let expiration =
            TdxAttestationHydrator::validate_collateral_freshness(&fetch, 150).unwrap();

        assert_eq!(expiration, 250);
    }

    #[test]
    fn collateral_freshness_rejects_expired_collateral() {
        let fetch = collateral_fetch(150, 250);

        let error = TdxAttestationHydrator::validate_collateral_freshness(&fetch, 150).unwrap_err();

        assert!(
            error
                .source()
                .expect("expired collateral error should be retained as the source")
                .to_string()
                .contains("TDX collateral is expired")
        );
    }

    #[test]
    fn collateral_freshness_rejects_malformed_collateral() {
        let mut fetch = collateral_fetch(300, 400);
        fetch.collateral.tcb_info.raw = Bytes::from_static(b"not-json");

        let error = TdxAttestationHydrator::validate_collateral_freshness(&fetch, 150).unwrap_err();

        assert!(
            error
                .source()
                .expect("malformed collateral error should be retained as the source")
                .to_string()
                .contains("TCB info collateral is invalid")
        );
    }

    #[test]
    fn quote_verification_time_uses_now_for_past_quote() {
        let verification_time =
            TdxAttestationHydrator::quote_verification_time_seconds(149_999, 150).unwrap();

        assert_eq!(verification_time, 150);
    }

    #[test]
    fn quote_verification_time_advances_same_second_quote() {
        let verification_time =
            TdxAttestationHydrator::quote_verification_time_seconds(150_999, 150).unwrap();

        assert_eq!(verification_time, 151);
    }

    #[test]
    fn quote_verification_time_rejects_future_quote() {
        let error =
            TdxAttestationHydrator::quote_verification_time_seconds(151_000, 150).unwrap_err();

        assert!(
            error
                .source()
                .expect("future timestamp error should be retained as the source")
                .to_string()
                .contains("in the future")
        );
    }
}
