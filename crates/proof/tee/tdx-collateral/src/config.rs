//! Configuration for Intel TDX collateral hydration.

use std::time::Duration;

use alloy_primitives::{B256, b256};
use base_proof_tee_tdx_verifier::TDXTcbStatus;
use reqwest::Url;

use crate::{Result, TdxCollateralError};

/// Default maximum accepted age for TDX quotes in seconds.
pub const DEFAULT_TDX_MAX_QUOTE_AGE_SECS: u64 = 300;

/// Default HTTP timeout for Intel PCS collateral and CRL fetches.
pub const DEFAULT_TDX_COLLATERAL_FETCH_TIMEOUT_SECS: u64 = 30;

/// Keccak-256 hash of Intel's production SGX/TDX Provisioning Certification
/// Root CA DER certificate for PCS API v4.
pub const DEFAULT_TDX_TRUSTED_ROOT_CA_HASH: B256 =
    b256!("a1acc73eb45794fa1734f14d882e91925b6006f79d3bb2460df9d01b333d7009");

/// Intel PCS and verifier policy configuration for TDX attestation hydration.
#[derive(Clone, Debug)]
pub struct TdxAttestationConfig {
    /// Intel TDX PCS API base URL.
    pub pcs_tdx_base_url: Url,
    /// Trusted Intel SGX/TDX root CA certificate hash expected by the verifier.
    pub trusted_root_ca_hash: B256,
    /// Maximum accepted TDX quote age.
    pub max_quote_age: Duration,
    /// Contract TCB statuses accepted by verifier policy.
    pub allowed_tcb_statuses: Vec<TDXTcbStatus>,
    /// HTTP timeout for Intel PCS collateral and CRL fetches.
    pub fetch_timeout: Duration,
}

impl TdxAttestationConfig {
    /// Production Intel PCS v4 endpoint with `UpToDate`-only TCB policy.
    pub fn intel_pcs() -> Self {
        Self {
            pcs_tdx_base_url: Url::parse(
                "https://api.trustedservices.intel.com/tdx/certification/v4/",
            )
            .expect("default Intel PCS URL must be valid"),
            trusted_root_ca_hash: DEFAULT_TDX_TRUSTED_ROOT_CA_HASH,
            max_quote_age: Duration::from_secs(DEFAULT_TDX_MAX_QUOTE_AGE_SECS),
            allowed_tcb_statuses: vec![TDXTcbStatus::UpToDate],
            fetch_timeout: Duration::from_secs(DEFAULT_TDX_COLLATERAL_FETCH_TIMEOUT_SECS),
        }
    }

    /// Builds an HTTP client configured for bounded TDX collateral fetching.
    pub fn build_http_client(&self) -> Result<reqwest::Client> {
        reqwest::Client::builder()
            .timeout(self.fetch_timeout)
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()
            .map_err(|e| TdxCollateralError::source(Box::new(e)))
    }
}
