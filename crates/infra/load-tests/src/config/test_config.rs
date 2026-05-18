use std::{fmt, path::Path, time::Duration};

use alloy_primitives::{Address, U256};
use alloy_signer_local::PrivateKeySigner;
use revm::precompile::PrecompileId;
use serde::{Deserialize, Deserializer, Serialize, de::Error as SerdeError};
use url::Url;

use crate::{
    metrics::ConfigSummary,
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
    /// HTTP JSON-RPC endpoints used for transaction submission.
    ///
    /// The first URL is also used for setup transactions such as funding, token distribution, and
    /// draining. A single scalar URL is accepted as a one-entry list.
    #[serde(default, deserialize_with = "deserialize_url_list")]
    pub transaction_submission_rpcs: Vec<Url>,
    /// HTTP JSON-RPC endpoint used for read/query operations.
    ///
    /// Defaults to the first entry in [`Self::transaction_submission_rpcs`] when omitted.
    #[serde(default)]
    pub query_rpc: Option<Url>,
    /// Optional HTTP JSON-RPC endpoints whose txpools should be cleared before a test.
    #[serde(default)]
    pub txpool_nodes: Vec<Url>,
    /// Builder flashblocks broadcast WebSocket endpoint.
    #[serde(default)]
    pub flashblocks_ws: Option<Url>,

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
    ///
    /// Defaults to `12345` for reproducible single-run testing and fund recovery. Concurrent runs
    /// against the same chain without overriding this will derive identical
    /// accounts and collide on nonces.
    pub seed: u64,

    /// Chain ID (if not provided, fetched from RPC).
    pub chain_id: Option<u64>,

    /// Transaction types with weights.
    pub transactions: Vec<WeightedTxType>,

    /// Address of the precompile looper contract (required when using iterations > 1).
    #[serde(default)]
    pub looper_contract: Option<String>,

    /// Amount of each swap token to distribute to each sender (in wei, as string).
    /// Only used when swap transaction types are configured.
    #[serde(default = "default_swap_token_amount")]
    pub swap_token_amount: String,
}

impl Default for TestConfig {
    fn default() -> Self {
        Self {
            transaction_submission_rpcs: vec![
                Url::parse("http://localhost:8545").expect("valid URL"),
            ],
            query_rpc: None,
            txpool_nodes: Vec::new(),
            flashblocks_ws: None,
            mnemonic: None,
            funding_amount: "10000000000000000".to_string(),
            sender_count: 100,
            sender_offset: 0,
            in_flight_per_sender: 256,
            batch_size: 50,
            batch_timeout: Some("100ms".to_string()),
            duration: Some("60s".to_string()),
            target_gps: Some(20_000_000),
            seed: 12345,
            chain_id: None,
            transactions: vec![WeightedTxType { weight: 100, tx_type: TxTypeConfig::Transfer }],
            looper_contract: None,
            swap_token_amount: default_swap_token_amount(),
        }
    }
}

impl fmt::Debug for TestConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestConfig")
            .field("transaction_submission_rpcs", &self.transaction_submission_rpcs)
            .field("query_rpc", &self.query_rpc)
            .field("txpool_nodes", &self.txpool_nodes)
            .field("flashblocks_ws", &self.flashblocks_ws)
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
            .field("swap_token_amount", &self.swap_token_amount)
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
    /// Uniswap V3 style swap.
    UniswapV3 {
        /// Router contract address.
        router: String,
        /// Input token address.
        token_in: String,
        /// Output token address.
        token_out: String,
        /// Fee tier (default 3000 = 0.3%).
        #[serde(default = "default_uniswap_v3_fee")]
        fee: u32,
        /// Minimum swap amount in wei.
        #[serde(default = "default_swap_min_amount")]
        min_amount: String,
        /// Maximum swap amount in wei.
        #[serde(default = "default_swap_max_amount")]
        max_amount: String,
    },
    /// Aerodrome Slipstream (concentrated liquidity) swap.
    AerodromeCl {
        /// CL Router contract address.
        router: String,
        /// Input token address.
        token_in: String,
        /// Output token address.
        token_out: String,
        /// Tick spacing for the pool.
        #[serde(default = "default_aerodrome_tick_spacing")]
        tick_spacing: i32,
        /// Minimum swap amount in wei.
        #[serde(default = "default_swap_min_amount")]
        min_amount: String,
        /// Maximum swap amount in wei.
        #[serde(default = "default_swap_max_amount")]
        max_amount: String,
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

fn default_swap_min_amount() -> String {
    "1000000000000000".to_string()
}

fn default_swap_max_amount() -> String {
    "10000000000000000".to_string()
}

const fn default_uniswap_v3_fee() -> u32 {
    3000
}

const fn default_aerodrome_tick_spacing() -> i32 {
    100
}

fn default_swap_token_amount() -> String {
    "1000000000000000000000".to_string() // 1000 tokens (1000e18)
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

        if self.transaction_submission_rpcs.is_empty() {
            return Err(BaselineError::Config(
                "transaction_submission_rpcs must not be empty".into(),
            ));
        }
        for url in &self.transaction_submission_rpcs {
            Self::validate_http_url(url, "transaction_submission_rpcs")?;
        }

        if let Some(url) = &self.query_rpc {
            Self::validate_http_url(url, "query_rpc")?;
        }
        for url in &self.txpool_nodes {
            Self::validate_http_url(url, "txpool_nodes")?;
        }

        let Some(flashblocks_ws) = &self.flashblocks_ws else {
            return Err(BaselineError::Config("flashblocks_ws is required".into()));
        };
        Self::validate_ws_url(flashblocks_ws, "flashblocks_ws")?;

        Ok(())
    }

    /// Returns the first transaction submission endpoint.
    pub fn primary_submission_rpc(&self) -> Result<&Url> {
        self.transaction_submission_rpcs.first().ok_or_else(|| {
            BaselineError::Config("transaction_submission_rpcs must not be empty".into())
        })
    }

    fn validate_http_url(url: &Url, field_name: &str) -> Result<()> {
        match url.scheme() {
            "http" | "https" => Ok(()),
            "ws" => Err(BaselineError::Config(format!(
                "{field_name} uses 'ws://' scheme but requires 'http://' for JSON-RPC requests"
            ))),
            "wss" => Err(BaselineError::Config(format!(
                "{field_name} uses 'wss://' scheme but requires 'https://' for JSON-RPC requests"
            ))),
            scheme => Err(BaselineError::Config(format!(
                "{field_name} has invalid scheme '{scheme}', expected 'http://' or 'https://'"
            ))),
        }
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

    /// Parses the swap token amount string into a U256.
    pub fn parse_swap_token_amount(&self) -> Result<alloy_primitives::U256> {
        self.swap_token_amount.parse().map_err(|e| {
            BaselineError::Config(format!(
                "invalid swap_token_amount '{}': {e}",
                self.swap_token_amount
            ))
        })
    }

    /// Returns a summary of the config for JSON output (excludes URLs and secrets).
    pub fn to_summary(&self) -> ConfigSummary {
        ConfigSummary {
            funding_amount: self.funding_amount.clone(),
            sender_count: self.sender_count,
            sender_offset: self.sender_offset,
            in_flight_per_sender: self.in_flight_per_sender,
            batch_size: self.batch_size,
            batch_timeout: self.batch_timeout.clone(),
            duration: self.duration.clone(),
            target_gps: self.target_gps,
            seed: self.seed,
            chain_id: self.chain_id,
            transactions: serde_json::to_value(&self.transactions)
                .inspect_err(|e| {
                    tracing::warn!(
                        error = %e,
                        "failed to serialize transactions for config summary"
                    );
                })
                .unwrap_or_default(),
            looper_contract: self.looper_contract.clone(),
            swap_token_amount: self.swap_token_amount.clone(),
        }
    }

    /// Converts this test config into a `LoadConfig` for runtime use.
    pub fn to_load_config(
        &self,
        fallback_chain_id: Option<u64>,
    ) -> Result<crate::runner::LoadConfig> {
        let resolved_chain_id = self.chain_id.or(fallback_chain_id).ok_or_else(|| {
            BaselineError::Config("chain_id must be provided in config or fetched from RPC".into())
        })?;

        let transaction_submission_rpcs = self.transaction_submission_rpcs.clone();
        let primary_submission_rpc = self.primary_submission_rpc()?.clone();
        let query_rpc = self.query_rpc.clone().unwrap_or(primary_submission_rpc);

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
            transaction_submission_rpcs,
            query_rpc,
            txpool_nodes: self.txpool_nodes.clone(),
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
            flashblocks_ws: self
                .flashblocks_ws
                .clone()
                .ok_or_else(|| BaselineError::Config("flashblocks_ws is required".into()))?,
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
            TxTypeConfig::UniswapV3 {
                router,
                token_in,
                token_out,
                fee,
                min_amount,
                max_amount,
            } => {
                let router = parse_address(router, "uniswap_v3 router")?;
                let token_in = parse_address(token_in, "uniswap_v3 token_in")?;
                let token_out = parse_address(token_out, "uniswap_v3 token_out")?;
                let max_u24: u32 = (1 << 24) - 1;
                if *fee > max_u24 {
                    return Err(BaselineError::Config(format!(
                        "uniswap_v3 fee {fee} exceeds u24 max ({max_u24})"
                    )));
                }
                let min_amount = parse_amount(min_amount, "uniswap_v3 min_amount")?;
                let max_amount = parse_amount(max_amount, "uniswap_v3 max_amount")?;
                validate_swap_amounts(min_amount, max_amount, "uniswap_v3")?;
                TxType::UniswapV3 { router, token_in, token_out, fee: *fee, min_amount, max_amount }
            }
            TxTypeConfig::AerodromeCl {
                router,
                token_in,
                token_out,
                tick_spacing,
                min_amount,
                max_amount,
            } => {
                let router = parse_address(router, "aerodrome_cl router")?;
                let token_in = parse_address(token_in, "aerodrome_cl token_in")?;
                let token_out = parse_address(token_out, "aerodrome_cl token_out")?;
                let min_amount = parse_amount(min_amount, "aerodrome_cl min_amount")?;
                let max_amount = parse_amount(max_amount, "aerodrome_cl max_amount")?;
                validate_swap_amounts(min_amount, max_amount, "aerodrome_cl")?;
                if !(-8_388_608..=8_388_607).contains(tick_spacing) {
                    return Err(BaselineError::Config(format!(
                        "aerodrome_cl tick_spacing {tick_spacing} exceeds i24 range"
                    )));
                }
                TxType::AerodromeCl {
                    router,
                    token_in,
                    token_out,
                    tick_spacing: *tick_spacing,
                    min_amount,
                    max_amount,
                }
            }
        };
        Ok(TxConfig { weight: weighted.weight, tx_type })
    }
}

/// Deserializes a URL field that may be written as either a scalar string or a list.
fn deserialize_url_list<'de, D>(deserializer: D) -> std::result::Result<Vec<Url>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_yaml::Value::deserialize(deserializer)?;
    match value {
        serde_yaml::Value::Null => Ok(Vec::new()),
        serde_yaml::Value::String(url) => Ok(vec![parse_url_for_serde(&url)?]),
        serde_yaml::Value::Sequence(urls) => urls
            .into_iter()
            .map(|value| match value {
                serde_yaml::Value::String(url) => parse_url_for_serde(&url),
                other => Err(D::Error::custom(format!(
                    "expected URL string in transaction_submission_rpcs, got {other:?}"
                ))),
            })
            .collect(),
        other => Err(D::Error::custom(format!(
            "expected URL string or list of URL strings, got {other:?}"
        ))),
    }
}

/// Parses a URL for serde deserialization.
fn parse_url_for_serde<E>(url: &str) -> std::result::Result<Url, E>
where
    E: SerdeError,
{
    Url::parse(url).map_err(|e| E::custom(format!("invalid URL '{url}': {e}")))
}

fn parse_address(s: &str, field: &str) -> Result<Address> {
    s.parse::<Address>()
        .map_err(|e| BaselineError::Config(format!("invalid {field} address '{s}': {e}")))
}

fn parse_amount(s: &str, field: &str) -> Result<U256> {
    s.parse::<U256>().map_err(|e| BaselineError::Config(format!("invalid {field} '{s}': {e}")))
}

fn validate_swap_amounts(min: U256, max: U256, tx_type: &str) -> Result<()> {
    if min > max {
        return Err(BaselineError::Config(format!(
            "{tx_type} min_amount ({min}) exceeds max_amount ({max})"
        )));
    }
    let u128_max = U256::from(u128::MAX);
    if min > u128_max {
        return Err(BaselineError::Config(format!(
            "{tx_type} min_amount ({min}) exceeds u128::MAX — swap calls require u128 amounts"
        )));
    }
    if max > u128_max {
        return Err(BaselineError::Config(format!(
            "{tx_type} max_amount ({max}) exceeds u128::MAX — swap calls require u128 amounts"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let yaml = r#"
transaction_submission_rpcs: http://localhost:8545
flashblocks_ws: ws://localhost:7111
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.primary_submission_rpc().unwrap().host_str(), Some("localhost"));
        assert_eq!(config.sender_count, 100);
        assert!(config.mnemonic.is_none());
        assert!(config.txpool_nodes.is_empty());
    }

    #[test]
    fn parse_sharded_submission_rpcs() {
        let yaml = r#"
transaction_submission_rpcs:
  - http://localhost:7545
  - http://localhost:7546
flashblocks_ws: ws://localhost:7111
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        let load_config = config.to_load_config(Some(1337)).unwrap();
        assert_eq!(load_config.transaction_submission_rpcs.len(), 2);
        assert_eq!(load_config.transaction_submission_rpcs[0].port(), Some(7545));
        assert_eq!(load_config.transaction_submission_rpcs[1].port(), Some(7546));
    }

    #[test]
    fn parse_txpool_nodes() {
        let yaml = r#"
transaction_submission_rpcs: http://localhost:8545
txpool_nodes:
  - http://localhost:7545
  - http://localhost:10545
flashblocks_ws: ws://localhost:7111
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        let load_config = config.to_load_config(Some(1337)).unwrap();
        assert_eq!(load_config.txpool_nodes.len(), 2);
        assert_eq!(load_config.txpool_nodes[0].port(), Some(7545));
        assert_eq!(load_config.txpool_nodes[1].port(), Some(10545));
    }

    #[test]
    fn parse_full_config() {
        let yaml = r#"
transaction_submission_rpcs: https://sepolia.base.org
flashblocks_ws: wss://sepolia.flashblocks.base.org/ws
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
transaction_submission_rpcs: http://localhost:8545
flashblocks_ws: ws://localhost:7111
duration: "30s"
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.parse_duration().unwrap().unwrap(), Duration::from_secs(30));

        let yaml2 = r#"
transaction_submission_rpcs: http://localhost:8545
flashblocks_ws: ws://localhost:7111
duration: "1h 30m"
"#;
        let config2 = TestConfig::from_yaml(yaml2).unwrap();
        assert_eq!(config2.parse_duration().unwrap().unwrap(), Duration::from_secs(5400));
    }

    #[test]
    fn parse_precompile_targets() {
        let yaml = r#"
transaction_submission_rpcs: http://localhost:8545
flashblocks_ws: ws://localhost:7111
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
transaction_submission_rpcs: http://localhost:8545
flashblocks_ws: ws://localhost:7111
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
    fn rejects_http_scheme_for_flashblocks_ws() {
        let yaml = r#"
transaction_submission_rpcs: http://localhost:8545
flashblocks_ws: http://localhost:7111
"#;
        let err = TestConfig::from_yaml(yaml).unwrap_err();
        assert!(err.to_string().contains("flashblocks_ws"));
        assert!(err.to_string().contains("ws://"));
    }

    #[test]
    fn query_rpc_accepts_http() {
        let yaml = r#"
transaction_submission_rpcs: http://localhost:8545
query_rpc: http://localhost:8546
flashblocks_ws: ws://localhost:7111
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.query_rpc.as_ref().unwrap().scheme(), "http");
    }

    #[test]
    fn query_rpc_rejects_wss() {
        let yaml = r#"
transaction_submission_rpcs: http://localhost:8545
query_rpc: wss://localhost:8546
flashblocks_ws: wss://localhost:7111
"#;
        let err = TestConfig::from_yaml(yaml).unwrap_err();
        assert!(err.to_string().contains("query_rpc"));
        assert!(err.to_string().contains("https://"));
    }

    #[test]
    fn txpool_nodes_rejects_ws() {
        let yaml = r#"
transaction_submission_rpcs: http://localhost:8545
txpool_nodes:
  - ws://localhost:7546
flashblocks_ws: ws://localhost:7111
"#;
        let err = TestConfig::from_yaml(yaml).unwrap_err();
        assert!(err.to_string().contains("txpool_nodes"));
        assert!(err.to_string().contains("http://"));
    }

    #[test]
    fn flashblocks_ws_accepts_wss() {
        let yaml = r#"
transaction_submission_rpcs: http://localhost:8545
flashblocks_ws: wss://localhost:7111
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.flashblocks_ws.as_ref().unwrap().scheme(), "wss");
    }

    #[test]
    fn missing_flashblocks_ws_is_rejected() {
        let yaml = r#"
transaction_submission_rpcs: http://localhost:8545
"#;
        let err = TestConfig::from_yaml(yaml).unwrap_err();
        assert!(err.to_string().contains("flashblocks_ws"));
    }

    #[test]
    fn parse_uniswap_v3_config() {
        let yaml = r#"
transaction_submission_rpcs: http://localhost:8545
flashblocks_ws: ws://localhost:7111
transactions:
  - weight: 10
    type: uniswap_v3
    router: "0xE592427A0AEce92De3Edee1F18E0157C05861564"
    token_in: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
    token_out: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
    fee: 500
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.transactions.len(), 1);
        match &config.transactions[0].tx_type {
            TxTypeConfig::UniswapV3 { fee, .. } => {
                assert_eq!(*fee, 500);
            }
            _ => panic!("expected UniswapV3"),
        }
    }

    #[test]
    fn parse_aerodrome_cl_config() {
        let yaml = r#"
transaction_submission_rpcs: http://localhost:8545
flashblocks_ws: ws://localhost:7111
transactions:
  - weight: 10
    type: aerodrome_cl
    router: "0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5"
    token_in: "0x4200000000000000000000000000000000000006"
    token_out: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
    tick_spacing: 200
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.transactions.len(), 1);
        match &config.transactions[0].tx_type {
            TxTypeConfig::AerodromeCl { tick_spacing, .. } => {
                assert_eq!(*tick_spacing, 200);
            }
            _ => panic!("expected AerodromeCl"),
        }
    }
}
