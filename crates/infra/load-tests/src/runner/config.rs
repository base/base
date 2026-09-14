use std::{path::PathBuf, time::Duration};

use alloy_primitives::{Address, U256};
use base_execution_txpool::ValidityOperator;
use revm::precompile::PrecompileId;
use url::Url;

use crate::{
    config::OsakaTarget,
    utils::{BaselineError, Result},
};

/// Source for a validity predicate's address, resolved per transaction at
/// prepare time (the concrete `from`/`to` are only known then).
#[derive(Debug, Clone)]
pub enum PredicateAddress {
    /// The transaction's sender (`from`).
    Sender,
    /// The transaction's recipient (`to`).
    Recipient,
    /// A fixed, pre-resolved address.
    Fixed(Address),
}

/// Source for a validity predicate's storage slot, resolved per transaction.
#[derive(Debug, Clone)]
pub enum SlotTemplate {
    /// A static slot index.
    Fixed(U256),
    /// A Solidity mapping slot `keccak256(key ++ mapping_slot)`.
    Mapping {
        /// Declared position of the mapping in contract storage.
        mapping_slot: U256,
        /// Mapping key address, resolved per transaction.
        key: PredicateAddress,
    },
}

/// Source for a storage predicate's comparison value, resolved per transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredicateValue {
    /// A fixed comparison value used by every transaction.
    Fixed(U256),
    /// The low bit of the transaction sender's address.
    ///
    /// This deterministically splits senders between values zero and one, which
    /// lets stress profiles keep both matching and parked transactions in the
    /// pool while a shared one-bit storage value changes.
    SenderParity,
}

impl PredicateValue {
    /// Resolves the comparison value for `sender`.
    pub fn resolve(self, sender: Address) -> U256 {
        match self {
            Self::Fixed(value) => value,
            Self::SenderParity => U256::from(sender.as_slice()[19] & 1),
        }
    }
}

/// Bound for a `block_number` validity predicate.
///
/// A block-number predicate may target a fixed absolute block height, or an
/// offset that is resolved against the current chain height at submission time
/// (`current_block + offset`). The offset form makes delayed-validity spikes
/// self-configuring: it accounts for the variable number of funding/setup blocks
/// that run before measured submission begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockNumberBound {
    /// A fixed, absolute block number used as-is.
    Absolute(U256),
    /// An offset added to the current block height when the template is
    /// resolved (`current_block + offset`).
    Offset(U256),
}

/// A runtime validity predicate template with literal values pre-parsed.
///
/// Addresses and slots may remain symbolic ([`PredicateAddress::Sender`],
/// [`SlotTemplate::Mapping`]) and are resolved into concrete
/// `ValidityPredicate` values against each transaction at prepare time.
#[derive(Debug, Clone)]
pub enum ValidityPredicateTemplate {
    /// Balance comparison template.
    Balance {
        /// Account whose balance is read.
        address: PredicateAddress,
        /// Comparison operator.
        op: ValidityOperator,
        /// Right-hand comparison value.
        value: U256,
    },
    /// Storage comparison template.
    Storage {
        /// Contract whose storage is read.
        address: PredicateAddress,
        /// Storage slot source.
        slot: SlotTemplate,
        /// Optional bit mask; `None` uses the server default (all ones).
        mask: Option<U256>,
        /// Comparison operator.
        op: ValidityOperator,
        /// Comparison value source.
        value: PredicateValue,
    },
    /// Block-number comparison template.
    ///
    /// Carries no address or slot: the block being built is read from the
    /// builder's context. The [`BlockNumberBound`] is either a fixed absolute
    /// value (same predicate for every transaction) or an offset resolved
    /// against the current chain height per prepare round.
    BlockNumber {
        /// Comparison operator.
        op: ValidityOperator,
        /// Right-hand comparison bound (absolute value or runtime offset).
        bound: BlockNumberBound,
    },
    /// Flashblock-index comparison template.
    ///
    /// Carries no address or slot: the flashblock being built is read from the
    /// builder's context, so this template resolves to the same predicate for
    /// every transaction.
    FlashblockIndex {
        /// Comparison operator.
        op: ValidityOperator,
        /// Right-hand comparison value.
        value: U256,
    },
}

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
    /// Storage-heavy contract write.
    Storage {
        /// Storage-writer contract address.
        contract: Address,
        /// Number of storage slots to write per transaction.
        slots_per_tx: u32,
    },
    /// Deterministic `DoubleCounter` `increment()` call.
    DoubleCounter {
        /// `DoubleCounter` contract address.
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
    /// B-20 precompile token transfer. Each sender creates its own token per run during setup and
    /// transfers it to a funded pair partner (alice <-> bob).
    B20,
    /// Osaka (Base Azul) opcode or precompile transaction.
    Osaka {
        /// Target Osaka feature.
        target: OsakaTarget,
    },
    /// Uniswap V3 style swap.
    UniswapV3 {
        /// Router contract address.
        router: Address,
        /// Input token address.
        token_in: Address,
        /// Output token address.
        token_out: Address,
        /// Fee tier.
        fee: u32,
        /// Minimum swap amount.
        min_amount: U256,
        /// Maximum swap amount.
        max_amount: U256,
        /// Minimum amount when swapping `token_out` to `token_in`.
        reverse_min_amount: U256,
        /// Maximum amount when swapping `token_out` to `token_in`.
        reverse_max_amount: U256,
    },
    /// Aerodrome Slipstream (concentrated liquidity) swap.
    AerodromeCl {
        /// CL Router contract address.
        router: Address,
        /// Input token address.
        token_in: Address,
        /// Output token address.
        token_out: Address,
        /// Tick spacing.
        tick_spacing: i32,
        /// Minimum swap amount.
        min_amount: U256,
        /// Maximum swap amount.
        max_amount: U256,
        /// Minimum amount when swapping `token_out` to `token_in`.
        reverse_min_amount: U256,
        /// Maximum amount when swapping `token_out` to `token_in`.
        reverse_max_amount: U256,
    },
}

/// Default maximum gas price cap (1000 gwei).
pub const DEFAULT_MAX_GAS_PRICE: u128 = 1_000_000_000_000;
/// Default per-sender in-flight limit, aligned with Reth's default account slots.
pub const DEFAULT_MAX_IN_FLIGHT_PER_SENDER: usize = 16;

/// Configuration for a load test run.
#[derive(Debug, Clone)]
pub struct LoadConfig {
    /// HTTP JSON-RPC endpoints used for sharded transaction submission.
    pub transaction_submission_rpcs: Vec<Url>,
    /// HTTP JSON-RPC endpoint used for read/query operations.
    pub query_rpc: Url,
    /// Optional HTTP JSON-RPC endpoints whose txpools should be cleared before a test.
    pub txpool_nodes: Vec<Url>,
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
    /// Optional gas-per-second target used to size each block's mempool floor.
    pub target_gps: Option<u64>,
    /// Optional block gas limit override used to size uncapped mempool inventory.
    pub block_gas_limit: Option<u64>,
    /// Expected cadence between canonical blocks.
    pub block_time: Duration,
    /// Benchmark-only control directory used to separate setup from measurement.
    pub separate_setup: Option<PathBuf>,
    /// Duration of the load test. `None` means run indefinitely until stopped.
    pub duration: Option<Duration>,
    /// Optional measured canonical block window size.
    pub measurement_blocks: Option<u64>,
    /// Maximum in-flight (unconfirmed) transactions per sender.
    pub max_in_flight_per_sender: usize,
    /// Optional ceiling on total in-flight (unconfirmed) transactions across all senders.
    ///
    /// Without this, the aggregate cap is implicitly `max_in_flight_per_sender *
    /// account_count`. Setting this bounds the open-loop headroom target
    /// independently of sender count, e.g. to protect a shared target node's
    /// mempool size regardless of how many senders are configured. `None` keeps
    /// the previous per-sender-derived behavior.
    pub max_total_in_flight: Option<usize>,
    /// Optional cap on concurrent outbound submission RPC requests across all
    /// sender workers.
    ///
    /// This throttles request *rate* to the submission endpoint(s) directly,
    /// independently of `max_in_flight_per_sender` / `max_total_in_flight`
    /// (which bound unconfirmed transactions, not outbound requests). Useful
    /// for staying under an RPC endpoint's rate limit without shrinking the
    /// in-flight inventory target. `None` leaves concurrency bounded by sender
    /// workers and the number of RPC chunks in each transaction batch.
    pub max_concurrent_submit_requests: Option<usize>,
    /// Maximum number of transactions in each JSON-RPC batch request.
    pub batch_size: usize,
    /// Maximum gas price cap to prevent overspending during congestion.
    pub max_gas_price: u128,
    /// Optional builder flashblocks WebSocket used for early inclusion signals.
    pub flashblocks_ws: Option<Url>,
    /// Fraction of transactions that draw a fresh recipient address instead of cycling through
    /// the sender pool. Used to drive account-trie fan-out for account-create workloads.
    pub fresh_recipient_ratio: f64,
    /// Fraction `0.0..=1.0` of senders routed through `base_sendRawTransactionValidity`.
    pub validity_ratio: f64,
    /// Predicate templates attached to each validity-bearing transaction.
    pub validity_predicates: Vec<ValidityPredicateTemplate>,
    /// Fraction of validity senders in the priority-lead cohort.
    pub validity_priority_lead_ratio: f64,
    /// Priority-tip multiplier for the validity priority-lead cohort.
    pub validity_priority_lead_multiplier: u128,
    /// Priority-tip divisor for validity-cohort measured transactions.
    pub validity_priority_fee_divisor: u128,
}

impl LoadConfig {
    /// Creates a new load config for devnet.
    pub fn devnet() -> Self {
        Self {
            transaction_submission_rpcs: vec![
                "http://localhost:8545".parse().expect("valid default transaction_submission_rpc"),
            ],
            query_rpc: "http://localhost:8545".parse().expect("valid default query_rpc"),
            txpool_nodes: Vec::new(),
            chain_id: 84538453,
            account_count: 10,
            seed: 42,
            mnemonic: None,
            sender_offset: 0,
            transactions: vec![TxConfig { weight: 100, tx_type: TxType::Transfer }],
            target_gps: None,
            block_gas_limit: None,
            block_time: Duration::from_secs(2),
            separate_setup: None,
            duration: Some(Duration::from_secs(30)),
            measurement_blocks: None,
            max_in_flight_per_sender: DEFAULT_MAX_IN_FLIGHT_PER_SENDER,
            max_total_in_flight: None,
            max_concurrent_submit_requests: None,
            batch_size: crate::rpc::MAX_BATCH_RPC_SIZE,
            max_gas_price: DEFAULT_MAX_GAS_PRICE,
            flashblocks_ws: None,
            fresh_recipient_ratio: 0.0,
            validity_ratio: 0.0,
            validity_predicates: Vec::new(),
            validity_priority_lead_ratio: 0.0,
            validity_priority_lead_multiplier: 1,
            validity_priority_fee_divisor: 1,
        }
    }

    /// Returns the first transaction submission endpoint.
    pub fn primary_submission_rpc(&self) -> &Url {
        self.transaction_submission_rpcs
            .first()
            .expect("LoadConfig::validate guarantees at least one submission RPC")
    }

    /// Validates the configuration, returning an error if invalid.
    pub fn validate(&self) -> Result<()> {
        if self.account_count == 0 {
            return Err(BaselineError::Config("account_count must be > 0".into()));
        }
        if self.target_gps == Some(0) {
            return Err(BaselineError::Config("target_gps must be > 0 when set".into()));
        }
        if self.block_gas_limit == Some(0) {
            return Err(BaselineError::Config("block_gas_limit must be > 0 when set".into()));
        }
        if self.block_time.is_zero() {
            return Err(BaselineError::Config("block_time must be > 0".into()));
        }
        if self.max_in_flight_per_sender == 0 {
            return Err(BaselineError::Config("max_in_flight_per_sender must be > 0".into()));
        }
        if self.max_total_in_flight == Some(0) {
            return Err(BaselineError::Config("max_total_in_flight must be > 0 when set".into()));
        }
        if self.max_concurrent_submit_requests == Some(0) {
            return Err(BaselineError::Config(
                "max_concurrent_submit_requests must be > 0 when set".into(),
            ));
        }
        if self.batch_size == 0 {
            return Err(BaselineError::Config("batch_size must be > 0".into()));
        }
        if self.duration == Some(Duration::ZERO) {
            return Err(BaselineError::Config(
                "duration must be > 0 (or omit for continuous)".into(),
            ));
        }
        if self.measurement_blocks == Some(0) {
            return Err(BaselineError::Config("measurement_blocks must be > 0 when set".into()));
        }
        if !(0.0..=1.0).contains(&self.fresh_recipient_ratio) {
            return Err(BaselineError::Config(
                "fresh_recipient_ratio must be between 0.0 and 1.0".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.validity_ratio) {
            return Err(BaselineError::Config("validity_ratio must be between 0.0 and 1.0".into()));
        }
        if !(0.0..=1.0).contains(&self.validity_priority_lead_ratio) {
            return Err(BaselineError::Config(
                "validity_priority_lead_ratio must be between 0.0 and 1.0".into(),
            ));
        }
        if self.validity_priority_lead_multiplier < 1 {
            return Err(BaselineError::Config(
                "validity_priority_lead_multiplier must be >= 1".into(),
            ));
        }
        if self.validity_priority_fee_divisor < 1 {
            return Err(BaselineError::Config("validity_priority_fee_divisor must be >= 1".into()));
        }
        if self.validity_predicates.len() > base_execution_txpool::DEFAULT_MAX_VALIDITY_PREDICATES {
            return Err(BaselineError::Config(format!(
                "validity_predicates exceeds the maximum of {}",
                base_execution_txpool::DEFAULT_MAX_VALIDITY_PREDICATES
            )));
        }
        if self.validity_ratio > 0.0 && self.validity_predicates.is_empty() {
            return Err(BaselineError::Config(
                "validity_predicates must be non-empty when validity_ratio > 0".into(),
            ));
        }
        if self.transactions.is_empty() {
            return Err(BaselineError::Config("transactions must not be empty".into()));
        }
        if self.transaction_submission_rpcs.is_empty() {
            return Err(BaselineError::Config(
                "transaction_submission_rpcs must not be empty".into(),
            ));
        }
        for url in &self.transaction_submission_rpcs {
            if !matches!(url.scheme(), "http" | "https") {
                return Err(BaselineError::Config(
                    "transaction_submission_rpcs must use http:// or https://".into(),
                ));
            }
        }
        if !matches!(self.query_rpc.scheme(), "http" | "https") {
            return Err(BaselineError::Config("query_rpc must use http:// or https://".into()));
        }
        for url in &self.txpool_nodes {
            if !matches!(url.scheme(), "http" | "https") {
                return Err(BaselineError::Config(
                    "txpool_nodes must use http:// or https://".into(),
                ));
            }
        }
        if self.flashblocks_ws.as_ref().is_some_and(|url| !matches!(url.scheme(), "ws" | "wss")) {
            return Err(BaselineError::Config("flashblocks_ws must use ws:// or wss://".into()));
        }
        Ok(())
    }

    /// Sets the transaction submission HTTP JSON-RPC URL.
    pub fn with_rpc_http_url(mut self, url: Url) -> Self {
        self.transaction_submission_rpcs = vec![url.clone()];
        self.query_rpc = url;
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

    /// Sets an optional gas-per-second ceiling.
    pub const fn with_target_gps(mut self, gps: Option<u64>) -> Self {
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
        self.measurement_blocks = None;
        self
    }

    /// Sets the maximum in-flight transactions per sender.
    pub const fn with_max_in_flight_per_sender(mut self, max: usize) -> Self {
        self.max_in_flight_per_sender = max;
        self
    }

    /// Sets an optional ceiling on total in-flight transactions across all senders.
    pub const fn with_max_total_in_flight(mut self, max: Option<usize>) -> Self {
        self.max_total_in_flight = max;
        self
    }

    /// Sets an optional cap on concurrent outbound submission RPC requests.
    pub const fn with_max_concurrent_submit_requests(mut self, max: Option<usize>) -> Self {
        self.max_concurrent_submit_requests = max;
        self
    }

    /// Returns the effective in-flight capacity for `account_count` senders: the
    /// per-sender limit multiplied by the sender count, clamped by
    /// [`Self::max_total_in_flight`] when set.
    pub fn effective_in_flight_capacity(&self, account_count: usize) -> usize {
        let per_sender_capacity = self.max_in_flight_per_sender.saturating_mul(account_count);
        self.max_total_in_flight
            .map_or(per_sender_capacity, |max_total| per_sender_capacity.min(max_total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_in_flight_capacity_defaults_to_per_sender_times_count() {
        let config = LoadConfig::devnet().with_max_in_flight_per_sender(128);
        assert_eq!(config.effective_in_flight_capacity(10), 1280);
    }

    #[test]
    fn effective_in_flight_capacity_clamps_to_max_total_in_flight() {
        let config = LoadConfig::devnet()
            .with_max_in_flight_per_sender(128)
            .with_max_total_in_flight(Some(500));
        assert_eq!(config.effective_in_flight_capacity(10), 500, "clamped below per-sender total");
        assert_eq!(config.effective_in_flight_capacity(2), 256, "per-sender total stays below cap");
    }

    #[test]
    fn validate_rejects_zero_max_total_in_flight() {
        let config = LoadConfig::devnet().with_max_total_in_flight(Some(0));
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_max_concurrent_submit_requests() {
        let config = LoadConfig::devnet().with_max_concurrent_submit_requests(Some(0));
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_accepts_max_concurrent_submit_requests() {
        let config = LoadConfig::devnet().with_max_concurrent_submit_requests(Some(4));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_measurement_blocks() {
        let mut config = LoadConfig::devnet();
        config.measurement_blocks = Some(0);
        assert!(config.validate().is_err());
    }

    #[test]
    fn with_continuous_clears_duration_and_measurement_blocks() {
        let mut config = LoadConfig::devnet();
        config.measurement_blocks = Some(250);

        let continuous = config.with_continuous();

        assert_eq!(continuous.duration, None);
        assert_eq!(continuous.measurement_blocks, None);
    }
}
