//! Command-line parsing for the TDX image hash tool.

use std::time::Duration;

use alloy_primitives::{Address, B256};
use base_proof_tee_tdx_collateral::TdxAttestationConfig;
use base_proof_tee_tdx_verifier::TDXTcbStatus;
use clap::{Args, Parser, ValueEnum};
use eyre::Result;
use url::Url;

use crate::{OnchainRegistryConfig, TdxImageHashTool};

/// TDX image hash inspection command.
#[derive(Debug, Parser)]
#[command(author, version)]
pub struct Cli {
    /// TDX prover JSON-RPC endpoint.
    #[arg(long, env = "TDX_PROVER_ENDPOINT")]
    pub endpoint: Url,

    /// Signer index to inspect when a prover returns multiple signer entries.
    #[arg(long, env = "TDX_SIGNER_INDEX", default_value_t = 0)]
    pub signer_index: usize,

    /// Verify the quote locally using the Intel PCS collateral provider.
    #[arg(long, env = "TDX_VERIFY_QUOTE")]
    pub verify_quote: bool,

    /// Collateral and verifier policy arguments.
    #[command(flatten)]
    pub collateral: CollateralArgs,

    /// Optional on-chain registry comparison arguments.
    #[command(flatten)]
    pub registry: RegistryArgs,
}

impl Cli {
    /// Runs the command and prints the inspection report.
    pub async fn run(self) -> Result<()> {
        let report = TdxImageHashTool::run(self.config()).await?;
        println!("{report}");
        Ok(())
    }

    /// Converts parsed CLI arguments into the library configuration.
    pub fn config(&self) -> crate::TdxImageHashConfig {
        crate::TdxImageHashConfig {
            endpoint: self.endpoint.clone(),
            signer_index: self.signer_index,
            verify_quote: self.verify_quote,
            attestation: self.collateral.config(),
            registry: self.registry.config(),
        }
    }
}

/// Intel PCS collateral and verifier policy arguments.
#[derive(Debug, Args)]
pub struct CollateralArgs {
    /// Intel TDX PCS API base URL.
    #[arg(long, env = "TDX_PCS_TDX_BASE_URL")]
    pub pcs_tdx_base_url: Option<Url>,

    /// Trusted Intel SGX/TDX root CA certificate hash.
    #[arg(long, env = "TDX_TRUSTED_ROOT_CA_HASH")]
    pub trusted_root_ca_hash: Option<B256>,

    /// Maximum accepted quote age in seconds.
    #[arg(long, env = "TDX_MAX_QUOTE_AGE_SECS")]
    pub max_quote_age_secs: Option<u64>,

    /// Allowed TDX TCB status. Repeat to allow multiple statuses.
    #[arg(long, env = "TDX_ALLOWED_TCB_STATUS", value_enum)]
    pub allowed_tcb_status: Vec<TdxTcbStatusArg>,

    /// Intel PCS and CRL fetch timeout in seconds.
    #[arg(long, env = "TDX_COLLATERAL_FETCH_TIMEOUT_SECS")]
    pub fetch_timeout_secs: Option<u64>,
}

impl CollateralArgs {
    /// Builds the registrar-compatible TDX attestation configuration.
    pub fn config(&self) -> TdxAttestationConfig {
        let mut config = TdxAttestationConfig::intel_pcs();
        if let Some(pcs_tdx_base_url) = &self.pcs_tdx_base_url {
            config.pcs_tdx_base_url = pcs_tdx_base_url.clone();
        }
        if let Some(trusted_root_ca_hash) = self.trusted_root_ca_hash {
            config.trusted_root_ca_hash = trusted_root_ca_hash;
        }
        if let Some(max_quote_age_secs) = self.max_quote_age_secs {
            config.max_quote_age = Duration::from_secs(max_quote_age_secs);
        }
        if !self.allowed_tcb_status.is_empty() {
            config.allowed_tcb_statuses =
                self.allowed_tcb_status.iter().map(|status| status.to_contract()).collect();
        }
        if let Some(fetch_timeout_secs) = self.fetch_timeout_secs {
            config.fetch_timeout = Duration::from_secs(fetch_timeout_secs);
        }
        config
    }
}

/// On-chain registry comparison arguments.
#[derive(Debug, Args)]
pub struct RegistryArgs {
    /// L1 RPC URL used to query `TEEProverRegistry`.
    #[arg(long, env = "L1_RPC_URL", requires = "registry_address")]
    pub l1_rpc_url: Option<Url>,

    /// `TEEProverRegistry` contract address.
    #[arg(long, env = "TEE_PROVER_REGISTRY_ADDRESS", requires = "l1_rpc_url")]
    pub registry_address: Option<Address>,
}

impl RegistryArgs {
    /// Builds optional on-chain registry comparison configuration.
    pub fn config(&self) -> Option<OnchainRegistryConfig> {
        Some(OnchainRegistryConfig {
            l1_rpc_url: self.l1_rpc_url.clone()?,
            registry_address: self.registry_address?,
        })
    }
}

/// CLI representation of contract TDX TCB statuses.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum TdxTcbStatusArg {
    /// Platform TCB is up to date.
    UpToDate,
    /// Platform needs software hardening.
    SwHardeningNeeded,
    /// Platform needs configuration hardening.
    ConfigurationNeeded,
    /// Platform needs configuration and software hardening.
    ConfigurationAndSwHardeningNeeded,
    /// Platform TCB is out of date.
    OutOfDate,
    /// Platform TCB is out of date and needs configuration hardening.
    OutOfDateConfigurationNeeded,
    /// Platform TCB has been revoked.
    Revoked,
}

impl TdxTcbStatusArg {
    /// Converts the CLI status into the contract enum value.
    pub const fn to_contract(self) -> TDXTcbStatus {
        match self {
            Self::UpToDate => TDXTcbStatus::UpToDate,
            Self::SwHardeningNeeded => TDXTcbStatus::SwHardeningNeeded,
            Self::ConfigurationNeeded => TDXTcbStatus::ConfigurationNeeded,
            Self::ConfigurationAndSwHardeningNeeded => {
                TDXTcbStatus::ConfigurationAndSwHardeningNeeded
            }
            Self::OutOfDate => TDXTcbStatus::OutOfDate,
            Self::OutOfDateConfigurationNeeded => TDXTcbStatus::OutOfDateConfigurationNeeded,
            Self::Revoked => TDXTcbStatus::Revoked,
        }
    }
}

#[cfg(test)]
mod tests {
    use base_proof_tee_tdx_collateral::DEFAULT_TDX_TRUSTED_ROOT_CA_HASH;
    use clap::Parser as _;

    use super::*;

    #[test]
    fn default_collateral_args_match_registrar_defaults() {
        let cli = Cli::parse_from([
            "base-proof-tee-tdx-image-hash",
            "--endpoint",
            "http://127.0.0.1:7310",
        ]);

        let config = cli.collateral.config();

        assert_eq!(config.trusted_root_ca_hash, DEFAULT_TDX_TRUSTED_ROOT_CA_HASH);
        assert_eq!(config.allowed_tcb_statuses, vec![TDXTcbStatus::UpToDate]);
    }

    #[test]
    fn parses_registry_args_together() {
        let cli = Cli::parse_from([
            "base-proof-tee-tdx-image-hash",
            "--endpoint",
            "http://127.0.0.1:7310",
            "--l1-rpc-url",
            "http://127.0.0.1:8545",
            "--registry-address",
            "0x0000000000000000000000000000000000000001",
        ]);

        let registry = cli.registry.config().unwrap();

        assert_eq!(registry.registry_address, Address::with_last_byte(1));
    }

    #[test]
    fn parses_repeated_allowed_tcb_statuses() {
        let cli = Cli::parse_from([
            "base-proof-tee-tdx-image-hash",
            "--endpoint",
            "http://127.0.0.1:7310",
            "--allowed-tcb-status",
            "up-to-date",
            "--allowed-tcb-status",
            "sw-hardening-needed",
        ]);

        let config = cli.collateral.config();

        assert_eq!(
            config.allowed_tcb_statuses,
            vec![TDXTcbStatus::UpToDate, TDXTcbStatus::SwHardeningNeeded]
        );
    }
}
