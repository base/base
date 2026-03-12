use std::path::Path;
use std::time::Duration;

use alloy_primitives::Address;
use serde::{Deserialize, Serialize};

use crate::runner::{TxConfig, TxType};
use crate::utils::{BaselineError, Result};
use crate::workload::PrecompileTarget;

/// Configuration for a load test, loadable from YAML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestConfig {
    /// RPC endpoint URL.
    pub rpc: String,

    /// Mnemonic phrase for deriving sender accounts.
    /// If not provided, accounts are generated from seed.
    #[serde(default)]
    pub mnemonic: Option<String>,

    /// Private key for the funder account (hex with 0x prefix).
    pub funder_key: String,

    /// Amount to fund each sender account (in wei, as string).
    #[serde(default = "default_funding_amount")]
    pub funding_amount: String,

    /// Number of sender accounts to create/use.
    #[serde(default = "default_sender_count")]
    pub sender_count: u32,

    /// Offset into mnemonic derivation path (skip first N accounts).
    #[serde(default)]
    pub sender_offset: u32,

    /// Maximum in-flight transactions per sender.
    #[serde(default = "default_in_flight_per_sender")]
    pub in_flight_per_sender: u32,

    /// Test duration (e.g., "30s", "5m", "1h").
    #[serde(default = "default_duration")]
    pub duration: Option<String>,

    /// Target transactions per second.
    #[serde(default = "default_target_tps")]
    pub target_tps: Option<u32>,

    /// Seed for deterministic account generation (used if mnemonic not provided).
    #[serde(default = "default_seed")]
    pub seed: u64,

    /// Chain ID (if not provided, fetched from RPC).
    #[serde(default)]
    pub chain_id: Option<u64>,

    /// Transaction types with weights.
    #[serde(default = "default_transactions")]
    pub transactions: Vec<WeightedTxType>,
}

/// A transaction type with its weight in the mix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeightedTxType {
    /// Weight for random selection (higher = more frequent).
    pub weight: u32,

    /// The transaction type configuration.
    #[serde(flatten)]
    pub tx_type: TxTypeConfig,
}

/// Transaction type configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TxTypeConfig {
    /// Simple ETH transfer.
    Transfer,

    /// ETH transfer with random calldata.
    Calldata {
        /// Maximum calldata size in bytes.
        #[serde(default = "default_calldata_size")]
        max_size: usize,
    },

    /// ERC20 token transfer (requires deployed contract).
    Erc20 {
        /// ERC20 contract address.
        contract: String,
    },

    /// Precompile call.
    Precompile {
        /// Target precompile (sha256, identity, ecrecover, etc.).
        #[serde(default = "default_precompile")]
        target: String,
    },
}

fn default_funding_amount() -> String {
    "100000000000000000".to_string()
}

const fn default_sender_count() -> u32 {
    10
}

const fn default_in_flight_per_sender() -> u32 {
    16
}

const fn default_seed() -> u64 {
    42
}

const fn default_calldata_size() -> usize {
    128
}

fn default_duration() -> Option<String> {
    Some("30s".to_string())
}

const fn default_target_tps() -> Option<u32> {
    Some(100)
}

fn default_precompile() -> String {
    "sha256".to_string()
}

fn default_transactions() -> Vec<WeightedTxType> {
    vec![WeightedTxType { weight: 100, tx_type: TxTypeConfig::Transfer }]
}

impl TestConfig {
    /// Loads configuration from a YAML file.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let contents = std::fs::read_to_string(path).map_err(|e| {
            BaselineError::Config(format!("failed to read config file {}: {}", path.display(), e))
        })?;
        Self::from_yaml(&contents)
    }

    /// Parses configuration from a YAML string.
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        serde_yaml::from_str(yaml)
            .map_err(|e| BaselineError::Config(format!("failed to parse YAML: {e}")))
    }

    /// Parses the duration string into a Duration.
    pub fn parse_duration(&self) -> Result<Option<Duration>> {
        self.duration
            .as_ref()
            .map(|d| {
                humantime::parse_duration(d.trim())
                    .map_err(|e| BaselineError::Config(format!("invalid duration '{d}': {e}")))
            })
            .transpose()
    }

    /// Parses the funding amount string into a U256.
    pub fn parse_funding_amount(&self) -> Result<alloy_primitives::U256> {
        self.funding_amount.parse().map_err(|e| {
            BaselineError::Config(format!("invalid funding_amount '{}': {e}", self.funding_amount))
        })
    }

    /// Converts this test config into a `LoadConfig` for runtime use.
    pub fn to_load_config(
        &self,
        fallback_chain_id: Option<u64>,
    ) -> Result<crate::runner::LoadConfig> {
        let resolved_chain_id = self.chain_id.or(fallback_chain_id).ok_or_else(|| {
            BaselineError::Config("chain_id must be provided in config or fetched from RPC".into())
        })?;

        let rpc_url = self
            .rpc
            .parse()
            .map_err(|e| BaselineError::Config(format!("invalid rpc url '{}': {}", self.rpc, e)))?;

        let duration = self.parse_duration()?.unwrap_or_else(|| Duration::from_secs(30));

        let transactions = if self.transactions.is_empty() {
            vec![TxConfig { weight: 100, tx_type: TxType::Transfer }]
        } else {
            self.transactions.iter().map(|t| self.convert_tx_type(t)).collect::<Result<Vec<_>>>()?
        };

        Ok(crate::runner::LoadConfig {
            rpc_url,
            chain_id: resolved_chain_id,
            account_count: self.sender_count as usize,
            seed: self.seed,
            mnemonic: self.mnemonic.clone(),
            sender_offset: self.sender_offset as usize,
            transactions,
            tps: self.target_tps.unwrap_or(100) as u64,
            duration,
            max_in_flight_per_sender: self.in_flight_per_sender as u64,
            batch_size: 5,
            batch_timeout: Duration::from_millis(50),
        })
    }

    fn convert_tx_type(&self, weighted: &WeightedTxType) -> Result<TxConfig> {
        let tx_type = match &weighted.tx_type {
            TxTypeConfig::Transfer => TxType::Transfer,
            TxTypeConfig::Calldata { max_size } => TxType::Calldata { max_size: *max_size },
            TxTypeConfig::Erc20 { contract } => {
                let address = contract.parse::<Address>().map_err(|e| {
                    BaselineError::Config(format!(
                        "invalid erc20 contract address '{contract}': {e}"
                    ))
                })?;
                TxType::Erc20 { contract: address }
            }
            TxTypeConfig::Precompile { target } => {
                let precompile = parse_precompile_target(target)?;
                TxType::Precompile { target: precompile }
            }
        };
        Ok(TxConfig { weight: weighted.weight, tx_type })
    }
}

fn parse_precompile_target(target: &str) -> Result<PrecompileTarget> {
    match target.to_lowercase().as_str() {
        "sha256" => Ok(PrecompileTarget::Sha256),
        "identity" => Ok(PrecompileTarget::Identity),
        "ecrecover" | "ec_recover" => Ok(PrecompileTarget::EcRecover),
        "ripemd160" | "ripemd" => Ok(PrecompileTarget::Ripemd160),
        "modexp" | "mod_exp" => Ok(PrecompileTarget::ModExp),
        "ecadd" | "ec_add" => Ok(PrecompileTarget::EcAdd),
        "ecmul" | "ec_mul" => Ok(PrecompileTarget::EcMul),
        "ecpairing" | "ec_pairing" => Ok(PrecompileTarget::EcPairing),
        "blake2f" | "blake2" => Ok(PrecompileTarget::Blake2f),
        _ => Err(BaselineError::Config(format!("unknown precompile target: {target}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let yaml = r#"
rpc: http://localhost:8545
funder_key: "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.rpc, "http://localhost:8545");
        assert_eq!(config.sender_count, 10);
        assert!(config.mnemonic.is_none());
    }

    #[test]
    fn parse_full_config() {
        let yaml = r#"
rpc: https://sepolia.base.org
mnemonic: "test test test test test test test test test test test junk"
funder_key: "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
funding_amount: "500000000000000000"
sender_count: 20
sender_offset: 5
in_flight_per_sender: 32
duration: "5m"
target_tps: 100
seed: 12345
transactions:
  - weight: 70
    type: transfer
  - weight: 20
    type: calldata
    max_size: 256
  - weight: 10
    type: precompile
    target: sha256
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.sender_count, 20);
        assert_eq!(config.sender_offset, 5);
        assert_eq!(config.transactions.len(), 3);

        let duration = config.parse_duration().unwrap().unwrap();
        assert_eq!(duration, Duration::from_secs(300));
    }

    #[test]
    fn parse_duration_formats() {
        let yaml = r#"
rpc: http://localhost:8545
funder_key: "0x1234"
duration: "30s"
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.parse_duration().unwrap().unwrap(), Duration::from_secs(30));

        let yaml2 = r#"
rpc: http://localhost:8545
funder_key: "0x1234"
duration: "1h 30m"
"#;
        let config2 = TestConfig::from_yaml(yaml2).unwrap();
        assert_eq!(config2.parse_duration().unwrap().unwrap(), Duration::from_secs(5400));
    }
}
