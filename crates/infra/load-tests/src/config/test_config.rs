use std::{fmt, path::Path, time::Duration};

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use rand::Rng;
use revm::precompile::PrecompileId;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    runner::{TxConfig, TxType},
    utils::{BaselineError, Result},
};

/// Typed precompile target for load test configuration.
///
/// Deserializes from a `target` string field with optional precompile-specific
/// parameters (e.g. `rounds` for blake2f).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum PrecompileTarget {
    /// ECDSA public key recovery (`ecrecover`, address `0x01`).
    Ecrecover,
    /// SHA-256 hash (`sha256`, address `0x02`).
    Sha256,
    /// RIPEMD-160 hash (`ripemd160`, address `0x03`).
    Ripemd160,
    /// Identity / data copy (`identity`, address `0x04`).
    Identity,
    /// Modular exponentiation (`modexp`, address `0x05`).
    Modexp,
    /// BN254 elliptic curve addition (`bn254_add`, address `0x06`).
    Bn254Add,
    /// BN254 scalar multiplication (`bn254_mul`, address `0x07`).
    Bn254Mul,
    /// BN254 pairing check (`bn254_pairing`, address `0x08`).
    Bn254Pairing,
    /// `BLAKE2f` compression (`blake2f`, address `0x09`).
    Blake2f {
        /// Fixed number of compression rounds. Random if `None`.
        #[serde(default)]
        rounds: Option<u32>,
    },
    /// KZG point evaluation (`kzg_point_evaluation`, address `0x0a`).
    #[serde(rename = "kzg_point_evaluation")]
    KzgPointEvaluation,
}

impl PrecompileTarget {
    /// Converts to the corresponding `revm` [`PrecompileId`].
    pub const fn to_precompile_id(&self) -> PrecompileId {
        match self {
            Self::Ecrecover => PrecompileId::EcRec,
            Self::Sha256 => PrecompileId::Sha256,
            Self::Ripemd160 => PrecompileId::Ripemd160,
            Self::Identity => PrecompileId::Identity,
            Self::Modexp => PrecompileId::ModExp,
            Self::Bn254Add => PrecompileId::Bn254Add,
            Self::Bn254Mul => PrecompileId::Bn254Mul,
            Self::Bn254Pairing => PrecompileId::Bn254Pairing,
            Self::Blake2f { .. } => PrecompileId::Blake2F,
            Self::KzgPointEvaluation => PrecompileId::KzgPointEvaluation,
        }
    }

    /// Returns the fixed blake2f round count, if configured.
    pub const fn blake2f_rounds(&self) -> Option<u32> {
        match self {
            Self::Blake2f { rounds } => *rounds,
            _ => None,
        }
    }
}

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

    /// Number of transactions to batch together before submitting to the RPC.
    pub batch_size: u32,

    /// Maximum time to wait for a batch to fill before flushing (e.g., "50ms", "200ms").
    pub batch_timeout: Option<String>,

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

    /// Address of the precompile looper contract (required when using iterations > 1).
    #[serde(default)]
    pub looper_contract: Option<String>,

    /// JSON-RPC endpoint for block tracking (WebSocket subscription or HTTP polling).
    #[serde(default = "default_block_watcher_url", alias = "rpc_ws_url", alias = "ws_url")]
    pub block_watcher_url: Url,
    /// WebSocket URL for flashblocks subscription.
    #[serde(default = "default_flashblocks_ws_url", alias = "flashblocks_url")]
    pub flashblocks_ws_url: Url,
}

impl Default for TestConfig {
    fn default() -> Self {
        Self {
            rpc: Url::parse("http://localhost:8545").expect("valid URL"),
            mnemonic: None,
            funding_amount: "10000000000000000".to_string(),
            sender_count: 50,
            sender_offset: 0,
            in_flight_per_sender: 64,
            batch_size: 20,
            batch_timeout: Some("100ms".to_string()),
            duration: Some("30s".to_string()),
            target_gps: Some(10_000_000),
            seed: rand::rng().random(),
            chain_id: None,
            transactions: vec![WeightedTxType { weight: 100, tx_type: TxTypeConfig::Transfer }],
            looper_contract: None,
            block_watcher_url: default_block_watcher_url(),
            flashblocks_ws_url: default_flashblocks_ws_url(),
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
            .field("looper_contract", &self.looper_contract)
            .field("block_watcher_url", &self.block_watcher_url)
            .field("flashblocks_ws_url", &self.flashblocks_ws_url)
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

/// Osaka (Base Azul) transaction target.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsakaTarget {
    /// CLZ opcode (EIP-7939): COUNT LEADING ZEROS — CREATE transaction with CLZ initcode.
    Clz,
    /// P256VERIFY precompile at 0x0100 with Osaka gas pricing 6 900 (EIP-7951).
    #[serde(rename = "p256verify_osaka")]
    P256verifyOsaka,
    /// MODEXP under Osaka rules: 1 024-byte field limit + min gas 500 (EIP-7823 + EIP-7883).
    ModexpOsaka,
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
        /// Target precompile configuration.
        #[serde(flatten)]
        target: PrecompileTarget,
        /// Number of iterations per transaction. Requires `looper_contract` when > 1.
        #[serde(default = "default_iterations")]
        iterations: u32,
    },

    /// Osaka (Base Azul) opcode or precompile transaction.
    Osaka {
        /// Target Osaka feature.
        target: OsakaTarget,
    },
}

const fn default_calldata_size() -> usize {
    128
}

const fn default_repeat_count() -> usize {
    1
}

const fn default_iterations() -> u32 {
    1
}

fn default_block_watcher_url() -> Url {
    Url::parse("ws://localhost:8546").expect("valid default block_watcher_url")
}

fn default_flashblocks_ws_url() -> Url {
    Url::parse("ws://localhost:7111").expect("valid default flashblocks_ws_url")
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

        Self::validate_ws_url(&self.flashblocks_ws_url, "flashblocks_ws_url")?;

        Ok(())
    }

    fn validate_ws_url(url: &Url, field_name: &str) -> Result<()> {
        match url.scheme() {
            "ws" | "wss" => Ok(()),
            "http" => Err(BaselineError::Config(format!(
                "{field_name} uses 'http://' scheme but requires 'ws://' for WebSocket connections"
            ))),
            "https" => Err(BaselineError::Config(format!(
                "{field_name} uses 'https://' scheme but requires 'wss://' for secure WebSocket connections"
            ))),
            scheme => Err(BaselineError::Config(format!(
                "{field_name} has invalid scheme '{scheme}', expected 'ws://' or 'wss://'"
            ))),
        }
    }

    /// Returns the funder key from the `FUNDER_KEY` environment variable.
    pub fn funder_key() -> Result<PrivateKeySigner> {
        Self::resolve_funder_key(None)
    }

    /// Resolves the funder key from an explicit override string, falling back to the
    /// `FUNDER_KEY` environment variable when no override is provided.
    pub fn resolve_funder_key(override_key: Option<&str>) -> Result<PrivateKeySigner> {
        let key_str = if let Some(s) = override_key {
            s.to_string()
        } else {
            std::env::var("FUNDER_KEY").map_err(|_| {
                BaselineError::Config("FUNDER_KEY environment variable is required".into())
            })?
        };
        key_str.parse().map_err(|e| {
            BaselineError::Config(format!("invalid funder key (expected 0x-prefixed hex): {e}"))
        })
    }

    /// Returns the checksummed funder address string, if the key resolves successfully.
    ///
    /// Checks the override first, then falls back to `FUNDER_KEY` env var.
    pub fn funder_key_address(override_key: Option<&str>) -> Option<String> {
        Self::resolve_funder_key(override_key).ok().map(|s| s.address().to_string())
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

        let rpc_http_url = self.rpc.clone();

        let duration = self.parse_duration()?;

        let transactions = if self.transactions.is_empty() {
            vec![TxConfig { weight: 100, tx_type: TxType::Transfer }]
        } else {
            self.transactions.iter().map(|t| self.convert_tx_type(t)).collect::<Result<Vec<_>>>()?
        };

        let batch_timeout = self
            .batch_timeout
            .as_ref()
            .map(|d| {
                humantime::parse_duration(d.trim())
                    .map_err(|e| BaselineError::Config(format!("invalid batch_timeout '{d}': {e}")))
            })
            .transpose()?
            .unwrap_or(Duration::from_millis(100));

        Ok(crate::runner::LoadConfig {
            rpc_http_url,
            chain_id: resolved_chain_id,
            account_count: self.sender_count as usize,
            seed: self.seed,
            mnemonic: self.mnemonic.clone(),
            sender_offset: self.sender_offset as usize,
            transactions,
            target_gps: self.target_gps.unwrap_or(2_100_000),
            duration,
            max_in_flight_per_sender: self.in_flight_per_sender as u64,
            batch_size: self.batch_size.max(1) as usize,
            batch_timeout,
            max_gas_price: crate::runner::DEFAULT_MAX_GAS_PRICE,
            block_watcher_url: self.block_watcher_url.clone(),
            flashblocks_ws_url: self.flashblocks_ws_url.clone(),
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
            TxTypeConfig::Precompile { target, iterations } => {
                let looper_contract = if *iterations > 1 {
                    let addr_str = self.looper_contract.as_ref().ok_or_else(|| {
                        BaselineError::Config(
                            "looper_contract required when precompile iterations > 1".into(),
                        )
                    })?;
                    let addr = addr_str.parse::<Address>().map_err(|e| {
                        BaselineError::Config(format!(
                            "invalid looper_contract address '{addr_str}': {e}"
                        ))
                    })?;
                    Some(addr)
                } else {
                    None
                };
                TxType::Precompile {
                    target: target.to_precompile_id(),
                    blake2f_rounds: target.blake2f_rounds(),
                    iterations: *iterations,
                    looper_contract,
                }
            }
            TxTypeConfig::Osaka { target } => TxType::Osaka { target: target.clone() },
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
        assert_eq!(config.sender_count, 50);
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

    #[test]
    fn parse_precompile_targets() {
        let yaml = r#"
rpc: http://localhost:8545
funder_key: "0x1234"
transactions:
  - weight: 10
    type: precompile
    target: sha256
  - weight: 10
    type: precompile
    target: blake2f
  - weight: 10
    type: precompile
    target: blake2f
    rounds: 1000
  - weight: 10
    type: precompile
    target: ecrecover
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.transactions.len(), 4);

        match &config.transactions[0].tx_type {
            TxTypeConfig::Precompile { target, iterations } => {
                assert!(matches!(target, PrecompileTarget::Sha256));
                assert_eq!(*iterations, 1);
            }
            _ => panic!("expected Precompile"),
        }

        match &config.transactions[1].tx_type {
            TxTypeConfig::Precompile { target, iterations } => {
                assert!(matches!(target, PrecompileTarget::Blake2f { rounds: None }));
                assert_eq!(*iterations, 1);
            }
            _ => panic!("expected Precompile"),
        }

        match &config.transactions[2].tx_type {
            TxTypeConfig::Precompile { target, iterations } => {
                assert!(matches!(target, PrecompileTarget::Blake2f { rounds: Some(1000) }));
                assert_eq!(*iterations, 1);
            }
            _ => panic!("expected Precompile"),
        }

        match &config.transactions[3].tx_type {
            TxTypeConfig::Precompile { target, iterations } => {
                assert!(matches!(target, PrecompileTarget::Ecrecover));
                assert_eq!(*iterations, 1);
            }
            _ => panic!("expected Precompile"),
        }
    }

    #[test]
    fn parse_precompile_with_iterations() {
        let yaml = r#"
rpc: http://localhost:8545
funder_key: "0x1234"
looper_contract: "0x1234567890123456789012345678901234567890"
transactions:
  - weight: 10
    type: precompile
    target: sha256
    iterations: 50
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.transactions.len(), 1);

        match &config.transactions[0].tx_type {
            TxTypeConfig::Precompile { target, iterations } => {
                assert!(matches!(target, PrecompileTarget::Sha256));
                assert_eq!(*iterations, 50);
            }
            _ => panic!("expected Precompile"),
        }

        assert!(config.looper_contract.is_some());
    }

    #[test]
    fn rejects_http_scheme_for_flashblocks_url() {
        let yaml = r#"
rpc: http://localhost:8545
flashblocks_ws_url: http://localhost:7111
"#;
        let err = TestConfig::from_yaml(yaml).unwrap_err();
        assert!(err.to_string().contains("flashblocks_ws_url"));
        assert!(err.to_string().contains("ws://"));
    }

    #[test]
    fn block_watcher_url_accepts_http() {
        let yaml = r#"
rpc: http://localhost:8545
block_watcher_url: http://localhost:8546
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.block_watcher_url.scheme(), "http");
    }

    #[test]
    fn block_watcher_url_accepts_wss() {
        let yaml = r#"
rpc: http://localhost:8545
block_watcher_url: wss://localhost:8546
flashblocks_ws_url: wss://localhost:7111
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.block_watcher_url.scheme(), "wss");
        assert_eq!(config.flashblocks_ws_url.scheme(), "wss");
    }

    #[test]
    fn block_watcher_url_alias_rpc_ws_url() {
        let yaml = r#"
rpc: http://localhost:8545
rpc_ws_url: ws://localhost:8546
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.block_watcher_url.scheme(), "ws");
    }

    #[test]
    fn omitted_urls_use_defaults() {
        let yaml = r#"
rpc: http://localhost:8545
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.block_watcher_url.as_str(), "ws://localhost:8546/");
        assert_eq!(config.flashblocks_ws_url.as_str(), "ws://localhost:7111/");
    }
}
