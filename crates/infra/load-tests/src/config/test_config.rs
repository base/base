use std::{fmt, path::Path, time::Duration};

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use rand::Rng;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    runner::{TxConfig, TxType},
    utils::{BaselineError, Result},
    workload::parse_precompile_id,
};

/// Configuration for a load test, loadable from YAML.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TestConfig {
    /// RPC endpoint URL.
    pub rpc: Url,

    /// Mnemonic phrase for deriving sender accounts.
    /// If not provided, accounts are generated from seed.
    #[serde(skip_serializing)]
    pub mnemonic: Option<String>,

    /// Amount to fund each sender account (in wei, as string).
    pub funding_amount: String,

    /// Number of sender accounts to create/use.
    pub sender_count: u32,

    /// Offset into mnemonic derivation path (skip first N accounts).
    pub sender_offset: u32,

    /// Maximum in-flight transactions per sender.
    pub in_flight_per_sender: u32,

    /// Test duration (e.g., "30s", "5m", "1h").
    pub duration: Option<String>,

    /// Target gas per second.
    pub target_gps: Option<u64>,

    /// Seed for deterministic account generation (used if mnemonic not provided).
    pub seed: u64,

    /// Chain ID (if not provided, fetched from RPC).
    pub chain_id: Option<u64>,

    /// Transaction types with weights.
    pub transactions: Vec<WeightedTxType>,
}

impl Default for TestConfig {
    fn default() -> Self {
        Self {
            rpc: Url::parse("http://localhost:8545").expect("valid URL"),
            mnemonic: None,
            funding_amount: "100000000000000000".to_string(),
            sender_count: 10,
            sender_offset: 0,
            in_flight_per_sender: 16,
            duration: Some("30s".to_string()),
            target_gps: Some(2_100_000),
            seed: rand::rng().random(),
            chain_id: None,
            transactions: vec![WeightedTxType { weight: 100, tx_type: TxTypeConfig::Transfer }],
        }
    }
}

impl fmt::Debug for TestConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestConfig")
            .field("rpc", &self.rpc)
            .field("mnemonic", &self.mnemonic.as_ref().map(|_| "[REDACTED]"))
            .field("funding_amount", &self.funding_amount)
            .field("sender_count", &self.sender_count)
            .field("sender_offset", &self.sender_offset)
            .field("in_flight_per_sender", &self.in_flight_per_sender)
            .field("duration", &self.duration)
            .field("target_gps", &self.target_gps)
            .field("seed", &self.seed)
            .field("chain_id", &self.chain_id)
            .field("transactions", &self.transactions)
            .finish()
    }
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
        /// Number of times to repeat the random sequence for compressibility.
        #[serde(default = "default_repeat_count")]
        repeat_count: usize,
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

const fn default_calldata_size() -> usize {
    128
}

const fn default_repeat_count() -> usize {
    1
}

fn default_precompile() -> String {
    "sha256".to_string()
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
        let config: Self = serde_yaml::from_str(yaml)
            .map_err(|e| BaselineError::Config(format!("failed to parse YAML: {e}")))?;
        config.validate()?;
        Ok(config)
    }

    /// Validates that all required fields are set and values are sensible.
    pub fn validate(&self) -> Result<()> {
        if self.sender_count == 0 {
            return Err(BaselineError::Config("sender_count must be > 0".into()));
        }
        Ok(())
    }

    /// Returns the funder key from the `FUNDER_KEY` environment variable.
    pub fn funder_key() -> Result<PrivateKeySigner> {
        let key = std::env::var("FUNDER_KEY").map_err(|_| {
            BaselineError::Config("FUNDER_KEY environment variable is required".into())
        })?;
        key.parse().map_err(|e| {
            BaselineError::Config(format!("invalid FUNDER_KEY (expected 0x-prefixed hex): {e}"))
        })
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

        let rpc_url = self.rpc.clone();

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
            target_gps: self.target_gps.unwrap_or(2_100_000),
            duration,
            max_in_flight_per_sender: self.in_flight_per_sender as u64,
            batch_size: 5,
            batch_timeout: Duration::from_millis(50),
            max_gas_price: crate::runner::DEFAULT_MAX_GAS_PRICE,
        })
    }

    fn convert_tx_type(&self, weighted: &WeightedTxType) -> Result<TxConfig> {
        let tx_type = match &weighted.tx_type {
            TxTypeConfig::Transfer => TxType::Transfer,
            TxTypeConfig::Calldata { max_size, repeat_count } => {
                TxType::Calldata { max_size: *max_size, repeat_count: *repeat_count }
            }
            TxTypeConfig::Erc20 { contract } => {
                let address = contract.parse::<Address>().map_err(|e| {
                    BaselineError::Config(format!(
                        "invalid erc20 contract address '{contract}': {e}"
                    ))
                })?;
                TxType::Erc20 { contract: address }
            }
            TxTypeConfig::Precompile { target } => {
                let id = parse_precompile_id(target).map_err(BaselineError::Config)?;
                TxType::Precompile { target: id }
            }
        };
        Ok(TxConfig { weight: weighted.weight, tx_type })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let yaml = r#"
rpc: http://localhost:8545
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.rpc.host_str(), Some("localhost"));
        assert_eq!(config.sender_count, 10);
        assert!(config.mnemonic.is_none());
    }

    #[test]
    fn parse_full_config() {
        let yaml = r#"
rpc: https://sepolia.base.org
mnemonic: "test test test test test test test test test test test junk"
funding_amount: "500000000000000000"
sender_count: 20
sender_offset: 5
in_flight_per_sender: 32
duration: "5m"
target_gps: 2100000
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
duration: "30s"
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.parse_duration().unwrap().unwrap(), Duration::from_secs(30));

        let yaml2 = r#"
rpc: http://localhost:8545
duration: "1h 30m"
"#;
        let config2 = TestConfig::from_yaml(yaml2).unwrap();
        assert_eq!(config2.parse_duration().unwrap().unwrap(), Duration::from_secs(5400));
    }
}
