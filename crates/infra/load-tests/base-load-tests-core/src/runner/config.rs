use std::time::Duration;

use alloy_primitives::Address;
use url::Url;

use crate::{
    utils::{BaselineError, Result},
    workload::PrecompileTarget,
};

/// Configuration for a single transaction type with its weight.
#[derive(Debug, Clone)]
pub struct TxConfig {
    /// Weight for random selection (higher = more frequent).
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
    },
    /// ERC20 token transfer.
    Erc20 {
        /// ERC20 contract address.
        contract: Address,
    },
    /// Precompile call.
    Precompile {
        /// Target precompile.
        target: PrecompileTarget,
    },
}

/// Configuration for a load test run.
#[derive(Debug, Clone)]
pub struct LoadConfig {
    /// RPC endpoint URL.
    pub rpc_url: Url,
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
    /// Target transactions per second.
    pub tps: u64,
    /// Duration of the load test.
    pub duration: Duration,
    /// Maximum in-flight (unconfirmed) transactions per sender.
    pub max_in_flight_per_sender: u64,
    /// Number of transactions to batch together before submitting.
    pub batch_size: usize,
    /// Maximum time to wait for a batch to fill before flushing.
    pub batch_timeout: Duration,
}

impl LoadConfig {
    /// Creates a new load config for devnet.
    pub fn devnet() -> Self {
        Self {
            rpc_url: "http://localhost:8545".parse().unwrap(),
            chain_id: 1337,
            account_count: 10,
            seed: 42,
            mnemonic: None,
            sender_offset: 0,
            transactions: vec![TxConfig { weight: 100, tx_type: TxType::Transfer }],
            tps: 100,
            duration: Duration::from_secs(30),
            max_in_flight_per_sender: 50,
            batch_size: 5,
            batch_timeout: Duration::from_millis(50),
        }
    }

    /// Creates a new load config for Sepolia Alpha.
    pub fn sepolia_alpha() -> Self {
        Self {
            rpc_url: "https://base-sepolia-alpha.cbhq.net".parse().unwrap(),
            chain_id: 11763072,
            account_count: 10,
            seed: 42,
            mnemonic: None,
            sender_offset: 0,
            transactions: vec![TxConfig { weight: 100, tx_type: TxType::Transfer }],
            tps: 100,
            duration: Duration::from_secs(30),
            max_in_flight_per_sender: 50,
            batch_size: 5,
            batch_timeout: Duration::from_millis(50),
        }
    }

    /// Validates the configuration, returning an error if invalid.
    pub fn validate(&self) -> Result<()> {
        if self.account_count == 0 {
            return Err(BaselineError::Config("account_count must be > 0".into()));
        }
        if self.tps == 0 {
            return Err(BaselineError::Config("tps must be > 0".into()));
        }
        if self.duration.is_zero() {
            return Err(BaselineError::Config("duration must be > 0".into()));
        }
        if self.batch_size == 0 {
            return Err(BaselineError::Config("batch_size must be > 0".into()));
        }
        if self.transactions.is_empty() {
            return Err(BaselineError::Config("transactions must not be empty".into()));
        }
        Ok(())
    }

    /// Sets the RPC URL.
    pub fn with_rpc_url(mut self, rpc_url: Url) -> Self {
        self.rpc_url = rpc_url;
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

    /// Sets the target TPS.
    pub const fn with_tps(mut self, tps: u64) -> Self {
        self.tps = tps;
        self
    }

    /// Sets the test duration.
    pub const fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = duration;
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
