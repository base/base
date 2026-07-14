//! TDX prover querying, quote parsing, verification, and registry comparison.

use std::time::UNIX_EPOCH;

use alloy_primitives::{Address, keccak256};
use alloy_provider::RootProvider;
use base_proof_contracts::ITEEProverRegistry;
use base_proof_primitives::EnclaveApiClient;
use base_proof_tee_tdx_collateral::{TdxAttestationConfig, TdxAttestationHydrator};
use base_proof_tee_tdx_verifier::{TdxQuote, TdxSignerAttestation, TdxVerifier, TdxVerifierInput};
use eyre::{Context, Result, bail};
use jsonrpsee::http_client::HttpClientBuilder;
use url::Url;

use crate::{
    OnchainRegistryReport, QuoteVerificationReport, TdxImageHashReport, TdxMeasurementsReport,
};

/// Optional onchain registry comparison configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnchainRegistryConfig {
    /// L1 RPC URL used to query the registry.
    pub l1_rpc_url: Url,
    /// Registry contract address.
    pub registry_address: Address,
}

/// Runtime configuration for the TDX image hash tool.
#[derive(Debug, Clone)]
pub struct TdxImageHashConfig {
    /// TDX prover JSON-RPC endpoint.
    pub endpoint: Url,
    /// Signer index to inspect.
    pub signer_index: usize,
    /// Whether to perform full local quote verification.
    pub verify_quote: bool,
    /// Registrar-compatible TDX attestation collateral configuration.
    pub attestation: TdxAttestationConfig,
    /// Optional onchain registry comparison.
    pub registry: Option<OnchainRegistryConfig>,
}

/// TDX image hash inspection runner.
#[derive(Debug)]
pub struct TdxImageHashTool;

impl TdxImageHashTool {
    /// Queries the prover endpoint and returns a complete report.
    pub async fn run(config: TdxImageHashConfig) -> Result<TdxImageHashReport> {
        let attestation = Self::fetch_attestation(&config).await?;
        let public_key_hash = TdxVerifier::validate_public_key(&attestation.signer_public_key)
            .wrap_err("TDX signer public key is malformed")?;
        let signer_address = Address::from_slice(&public_key_hash.as_slice()[12..]);
        let parsed_quote =
            TdxQuote::parse(&attestation.quote).wrap_err("failed to parse TDX quote")?;
        TdxVerifier::verify_report_data(
            &parsed_quote,
            public_key_hash,
            attestation.quote_timestamp_millis,
            attestation.attestation_nonce,
        )
        .wrap_err("TDX quote report data does not bind the signer, timestamp, and nonce")?;

        let measurement_report = TdxMeasurementsReport {
            mr_td_hash: keccak256(parsed_quote.mrtd),
            rtmr0: parsed_quote.rtmr0,
            rtmr1: parsed_quote.rtmr1,
            rtmr2: parsed_quote.rtmr2,
            rtmr3: parsed_quote.rtmr3,
            image_hash: parsed_quote.image_hash(),
            report_data_suffix: parsed_quote.report_data_suffix(),
            quote_timestamp_millis: attestation.quote_timestamp_millis,
        };

        let quote_verification = if config.verify_quote {
            Some(Self::verify_quote(&config.attestation, signer_address, &attestation).await?)
        } else {
            None
        };

        let registry = if let Some(registry_config) = &config.registry {
            let registry_report = Self::query_registry(registry_config, signer_address)
                .await
                .wrap_err("failed to query onchain TEE prover registry for computed signer")?;
            if registry_report.is_registered_signer
                && registry_report.signer_image_hash != measurement_report.image_hash
            {
                bail!(
                    "registered signerImageHash {} does not match computed imageHash {}",
                    registry_report.signer_image_hash,
                    measurement_report.image_hash
                );
            }
            Some(registry_report)
        } else {
            None
        };

        Ok(TdxImageHashReport {
            signer_address,
            measurements: measurement_report,
            quote_verification,
            registry,
        })
    }

    async fn fetch_attestation(config: &TdxImageHashConfig) -> Result<TdxSignerAttestation> {
        let client = HttpClientBuilder::default()
            .request_timeout(config.attestation.fetch_timeout)
            .build(config.endpoint.as_str())
            .wrap_err_with(|| format!("failed to build JSON-RPC client for {}", config.endpoint))?;

        let attestations = client
            .signer_attestation(None, None)
            .await
            .wrap_err("failed to query signer attestations")?;
        let attestation_bytes = attestations.get(config.signer_index).ok_or_else(|| {
            eyre::eyre!(
                "signer index {} is out of range for {} attestations",
                config.signer_index,
                attestations.len()
            )
        })?;
        TdxSignerAttestation::decode(attestation_bytes)
            .wrap_err("failed to decode TDX signer attestation payload")
    }

    async fn verify_quote(
        attestation_config: &TdxAttestationConfig,
        signer_address: Address,
        attestation: &TdxSignerAttestation,
    ) -> Result<QuoteVerificationReport> {
        let hydrator =
            TdxAttestationHydrator::new(attestation_config.clone()).map_err(|error| {
                eyre::eyre!("failed to initialize TDX collateral provider: {error}")
            })?;
        let collateral = hydrator
            .fetch_collateral(&attestation.quote)
            .await
            .map_err(|error| eyre::eyre!("failed to fetch TDX collateral: {error}"))?;
        let verifier_input = TdxVerifierInput {
            quote: attestation.quote.clone(),
            pck_certificate_chain: collateral.pck_certificate_chain,
            collateral: collateral.collateral,
            revocation: collateral.revocation,
            trusted_root_ca_hash: attestation_config.trusted_root_ca_hash,
            expected_public_key: attestation.signer_public_key.clone(),
            attestation_nonce: attestation.attestation_nonce,
            quote_timestamp_millis: attestation.quote_timestamp_millis,
            verification_time: UNIX_EPOCH
                .elapsed()
                .wrap_err("system clock is before the Unix epoch")?
                .as_secs(),
            max_quote_age_seconds: attestation_config.max_quote_age.as_secs(),
            allowed_tcb_statuses: attestation_config.allowed_tcb_statuses.clone(),
        };
        let journal =
            TdxVerifier::verify(&verifier_input).wrap_err("local TDX quote verification failed")?;
        if journal.signer != signer_address {
            bail!("TDX quote signer mismatch: expected {signer_address}, got {}", journal.signer);
        }

        Ok(QuoteVerificationReport { collateral_expiration: journal.collateralExpiration })
    }

    async fn query_registry(
        config: &OnchainRegistryConfig,
        signer_address: Address,
    ) -> Result<OnchainRegistryReport> {
        let provider: RootProvider = RootProvider::new_http(config.l1_rpc_url.clone());
        let registry =
            ITEEProverRegistry::ITEEProverRegistryInstance::new(config.registry_address, provider);
        let signer_image_hash = registry
            .signerImageHash(signer_address)
            .call()
            .await
            .wrap_err("failed to read signerImageHash")?;
        let expected_image_hash = registry
            .getExpectedTDXImageHash()
            .call()
            .await
            .wrap_err("failed to read getExpectedTDXImageHash")?;
        let is_registered_signer = registry
            .isRegisteredSigner(signer_address)
            .call()
            .await
            .wrap_err("failed to read isRegisteredSigner")?;
        let is_valid_signer = registry
            .isValidSigner(signer_address)
            .call()
            .await
            .wrap_err("failed to read isValidSigner")?;

        Ok(OnchainRegistryReport {
            registry_address: config.registry_address,
            signer_image_hash,
            expected_image_hash,
            is_registered_signer,
            is_valid_signer,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Arc};

    use base_proof_tee_tdx_collateral::TdxAttestationConfig;
    use base_proof_tee_tdx_prover::{TdxMeasurements, TdxProverServer};
    use base_proof_tee_tdx_runtime::{TdxQuoteProvider, TdxRuntime};
    use jsonrpsee::server::Server;

    use super::*;

    #[tokio::test]
    async fn queries_mock_tdx_prover_and_computes_image_hash() {
        let runtime = Arc::new(TdxRuntime::new(TdxMeasurements));
        let module = TdxProverServer::new(runtime).into_rpc_module().unwrap();
        let server =
            Server::builder().build("127.0.0.1:0".parse::<SocketAddr>().unwrap()).await.unwrap();
        let addr = server.local_addr().unwrap();
        let handle = server.start(module);
        let endpoint = Url::parse(&format!("http://{addr}")).unwrap();

        let report = TdxImageHashTool::run(TdxImageHashConfig {
            endpoint,
            signer_index: 0,
            verify_quote: false,
            attestation: TdxAttestationConfig::intel_pcs(),
            registry: None,
        })
        .await
        .unwrap();

        handle.stop().unwrap();
        let measurements = TdxMeasurements;
        let quote = TdxQuote::parse(&measurements.quote(&[0; 64]).unwrap()).unwrap();
        assert_eq!(report.measurements.image_hash, quote.image_hash());
        assert_eq!(
            report.measurements.report_data_suffix,
            TdxVerifier::timestamp_report_data_suffix(
                report.measurements.quote_timestamp_millis,
                None
            )
        );
    }
}
