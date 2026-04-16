use std::time::Duration;

use alloy_primitives::Address;
use revm::precompile::PrecompileId;
use url::Url;

use crate::{
    config::OsakaTarget,
    utils::{BaselineError, Result},
};

/// Configuration for a single transaction type with its weight.
#[derive(Debug, Clone)]
pub struct TxConfig {
    /// Weight for transaction count selection (higher = more transactions of this type).
    /// Weights are relative: if Transfer has weight 70 and Calldata has weight 30,
    /// ~70% of generated transactions will be transfers.
    pub weight: u32,
    /// The transaction type details.
    pub tx_type: TxType,
}

/// Transaction type with its parameters.
#[derive(Debug, Clone)]
pub enum TxType {
    /// Simple ETH transfer.
    Transfer,
    /// ETH transfer with random calldata.
    Calldata {
        /// Maximum calldata size in bytes.
        max_size: usize,
        /// Repeat count for compressibility (1 = no repetition).
        repeat_count: usize,
    },
    /// ERC20 token transfer.
    Erc20 {
        /// ERC20 contract address.
        contract: Address,
    },
    /// Precompile call.
    Precompile {
        /// Target precompile.
        target: PrecompileId,
        /// Fixed number of rounds for Blake2f. If `None`, a random value is used.
        blake2f_rounds: Option<u32>,
        /// Number of iterations per transaction (requires looper contract when > 1).
        iterations: u32,
        /// Looper contract address (required when iterations > 1).
        looper_contract: Option<Address>,
    },
    /// Osaka (Base Azul) opcode or precompile transaction.
    Osaka {
        /// Target Osaka feature.
        target: OsakaTarget,
    },
}

/// Default maximum gas price cap (1000 gwei).
pub const DEFAULT_MAX_GAS_PRICE: u128 = 1_000_000_000_000;

/// Configuration for a load test run.
#[derive(Debug, Clone)]
pub struct LoadConfig {
    /// HTTP JSON-RPC endpoint URL.
    pub rpc_http_url: Url,
    /// Chain ID.
    pub chain_id: u64,
    /// Number of test accounts to create.
    pub account_count: usize,
    /// Seed for deterministic account generation (used if mnemonic is None).
    pub seed: u64,
    /// Mnemonic phrase for deriving sender accounts.
    pub mnemonic: Option<String>,
    /// Offset into account derivation (skip first N accounts).
    pub sender_offset: usize,
    /// Transaction types with weights.
    pub transactions: Vec<TxConfig>,
    /// Target gas per second.
    pub target_gps: u64,
    /// Duration of the load test. `None` means run indefinitely until stopped.
    pub duration: Option<Duration>,
    /// Maximum in-flight (unconfirmed) transactions per sender.
    pub max_in_flight_per_sender: u64,
    /// Number of transactions to batch together before submitting.
    pub batch_size: usize,
    /// Maximum time to wait for a batch to fill before flushing.
    pub batch_timeout: Duration,
    /// Maximum gas price cap to prevent overspending during congestion.
    pub max_gas_price: u128,
    /// WebSocket JSON-RPC endpoint URL for block subscription (enables block latency tracking).
    pub rpc_ws_url: Option<Url>,
    /// WebSocket URL for flashblocks subscription (enables flashblock latency tracking).
    pub flashblocks_ws_url: Option<Url>,
}

impl LoadConfig {
    /// Creates a new load config for devnet.
    pub fn devnet() -> Self {
        Self {
            rpc_http_url: "http://localhost:8545".parse().expect("valid default rpc_http_url"),
            chain_id: 1337,
            account_count: 10,
            seed: 42,
            mnemonic: None,
            sender_offset: 0,
            transactions: vec![TxConfig { weight: 100, tx_type: TxType::Transfer }],
            target_gps: 2_100_000,
            duration: Some(Duration::from_secs(30)),
            max_in_flight_per_sender: 50,
            batch_size: 5,
            batch_timeout: Duration::from_millis(50),
            max_gas_price: DEFAULT_MAX_GAS_PRICE,
            rpc_ws_url: Some("ws://localhost:8546".parse().expect("valid default rpc_ws_url")),
            flashblocks_ws_url: Some(
                "ws://localhost:7111".parse().expect("valid default flashblocks_ws_url"),
            ),
        }
    }

    /// Validates the configuration, returning an error if invalid.
    pub fn validate(&self) -> Result<()> {
        if self.account_count == 0 {
            return Err(BaselineError::Config("account_count must be > 0".into()));
        }
        if self.target_gps == 0 {
            return Err(BaselineError::Config("target_gps must be > 0".into()));
        }
        if self.duration == Some(Duration::ZERO) {
            return Err(BaselineError::Config(
                "duration must be > 0 (or omit for continuous)".into(),
            ));
        }
        if self.batch_size == 0 {
            return Err(BaselineError::Config("batch_size must be > 0".into()));
        }
        if self.transactions.is_empty() {
            return Err(BaselineError::Config("transactions must not be empty".into()));
        }
        Ok(())
    }

    /// Sets the HTTP JSON-RPC URL.
    pub fn with_rpc_http_url(mut self, url: Url) -> Self {
        self.rpc_http_url = url;
        self
    }

    /// Sets the chain ID.
    pub const fn with_chain_id(mut self, chain_id: u64) -> Self {
        self.chain_id = chain_id;
        self
    }

    /// Sets the number of test accounts.
    pub const fn with_account_count(mut self, count: usize) -> Self {
        self.account_count = count;
        self
    }

    /// Sets the seed for deterministic generation (only used if mnemonic is None).
    pub const fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// Sets the mnemonic for account derivation.
    pub fn with_mnemonic(mut self, mnemonic: impl Into<String>) -> Self {
        self.mnemonic = Some(mnemonic.into());
        self
    }

    /// Sets the sender offset (skip first N accounts in derivation).
    pub const fn with_sender_offset(mut self, offset: usize) -> Self {
        self.sender_offset = offset;
        self
    }

    /// Sets the transaction types with weights.
    pub fn with_transactions(mut self, transactions: Vec<TxConfig>) -> Self {
        self.transactions = transactions;
        self
    }

    /// Sets the target gas per second.
    pub const fn with_target_gps(mut self, gps: u64) -> Self {
        self.target_gps = gps;
        self
    }

    /// Sets the test duration.
    pub const fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = Some(duration);
        self
    }

    /// Sets the test to run indefinitely until stopped via the stop flag or Ctrl-C.
    pub const fn with_continuous(mut self) -> Self {
        self.duration = None;
        self
    }

    /// Sets the maximum in-flight transactions per sender.
    pub const fn with_max_in_flight_per_sender(mut self, max: u64) -> Self {
        self.max_in_flight_per_sender = max;
        self
    }

    /// Sets the batch size for transaction submission.
    pub const fn with_batch_size(mut self, size: usize) -> Self {
        self.batch_size = size;
        self
    }

    /// Sets the batch timeout.
    pub const fn with_batch_timeout(mut self, timeout: Duration) -> Self {
        self.batch_timeout = timeout;
        self
    }
}
