//! Additional Node command arguments.

//! clap [Args](clap::Args) for Base rollup configuration

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use base_execution_trie::{MdbxProofsStorageOptions, RocksdbProofsStorageOptions};
use base_upgrade_signal::{UpgradeSignalArgs, UpgradeSignalL1RpcArgs};
use clap::{ArgAction, ValueEnum, builder::ArgPredicate};

/// Default proofs history window: 1 month of blocks at 2s block time.
pub const DEFAULT_PROOFS_HISTORY_WINDOW_BLOCKS: u64 = 1_296_000;

/// Twelve hours of blocks at 2s block time.
pub const TWELVE_HOURS_IN_BLOCKS: u64 = 21_600;

const MIB: u64 = 1024 * 1024;
const DEFAULT_ROCKSDB_BLOCK_CACHE_SIZE_MIB: u64 = 1024;
const DEFAULT_ROCKSDB_BYTES_PER_SYNC_MIB: u64 = 1;
const DEFAULT_ROCKSDB_COMPACTION_READAHEAD_SIZE_MIB: u64 = 0;
const DEFAULT_ROCKSDB_LEVEL_ZERO_FILE_NUM_COMPACTION_TRIGGER: i32 = 4;
const DEFAULT_ROCKSDB_LEVEL_ZERO_SLOWDOWN_WRITES_TRIGGER: i32 = 20;
const DEFAULT_ROCKSDB_LEVEL_ZERO_STOP_WRITES_TRIGGER: i32 = 36;
const DEFAULT_ROCKSDB_MAX_BACKGROUND_JOBS: i32 = 4;
const DEFAULT_ROCKSDB_MAX_SUBCOMPACTIONS: u32 = 1;
const DEFAULT_ROCKSDB_MAX_WRITE_BUFFER_NUMBER: i32 = 3;
const DEFAULT_ROCKSDB_TARGET_FILE_SIZE_BASE_MIB: u64 = 256;
const DEFAULT_ROCKSDB_WRITE_BUFFER_SIZE_MIB: u64 = 64;

/// Transaction ordering strategy for the mempool.
///
/// Determines how transactions are prioritized when building blocks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum TxpoolOrdering {
    /// Order by coinbase tip (fee-based, higher tip = higher priority).
    ///
    /// This is the default ordering strategy that prioritizes transactions
    /// based on the priority fee (tip) they offer to the block producer.
    #[default]
    CoinbaseTip,
    /// Order by receive timestamp (FIFO, earlier = higher priority).
    ///
    /// Transactions are ordered by when they were received by the mempool,
    /// regardless of the fees they offer.
    Timestamp,
}

/// On-disk database backend for proofs history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ProofsHistoryDbBackend {
    /// Store proofs history in `RocksDB`. Also accepted as `v2`.
    #[value(alias = "v2")]
    Rocksdb,
    /// Store proofs history in `MDBX`.
    #[default]
    Mdbx,
}

impl ProofsHistoryDbBackend {
    /// Returns an error if an existing proofs-history directory belongs to a different backend.
    pub fn ensure_storage_path_matches(self, path: &Path) -> eyre::Result<()> {
        if !path.exists() {
            return Ok(());
        }

        let has_rocksdb_marker = path.join("CURRENT").exists();
        let has_mdbx_marker = path.join("mdbx.dat").exists();

        if has_rocksdb_marker && has_mdbx_marker {
            return Err(eyre::eyre!(
                "storage path contains both RocksDB marker CURRENT and MDBX marker mdbx.dat: {}",
                path.display()
            ));
        }

        match self {
            Self::Rocksdb if has_mdbx_marker => Err(eyre::eyre!(
                "proofs-history.db=rocksdb but storage path contains MDBX marker mdbx.dat: {}",
                path.display()
            )),
            Self::Mdbx if has_rocksdb_marker => Err(eyre::eyre!(
                "proofs-history.db=mdbx but storage path contains RocksDB marker CURRENT: {}",
                path.display()
            )),
            _ => Ok(()),
        }
    }
}

/// Runtime tuning options for the `MDBX` proofs history backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::Args)]
pub struct ProofsHistoryMdbxArgs {
    /// Maximum duration a read transaction can stay open.
    #[arg(
        long = "proofs-history.mdbx.max-read-transaction-duration",
        value_name = "PROOFS_HISTORY_MDBX_MAX_READ_TRANSACTION_DURATION",
        value_parser = parse_positive_duration,
        hide = true
    )]
    pub max_read_transaction_duration: Option<Duration>,
}

impl ProofsHistoryMdbxArgs {
    /// Converts CLI arguments into storage options.
    pub const fn storage_options(self) -> MdbxProofsStorageOptions {
        MdbxProofsStorageOptions {
            max_read_transaction_duration: self.max_read_transaction_duration,
        }
    }
}

fn parse_positive_duration(s: &str) -> Result<Duration, String> {
    let d = humantime::parse_duration(s).map_err(|e| e.to_string())?;
    if d.is_zero() {
        return Err("duration must be greater than zero".to_owned());
    }
    Ok(d)
}

/// Runtime tuning options for the `RocksDB` proofs history backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::Args)]
pub struct ProofsHistoryRocksdbArgs {
    /// `RocksDB` block cache size in `MiB`.
    #[arg(
        long = "proofs-history.rocksdb.block-cache-size-mib",
        value_name = "PROOFS_HISTORY_ROCKSDB_BLOCK_CACHE_SIZE_MIB",
        default_value_t = DEFAULT_ROCKSDB_BLOCK_CACHE_SIZE_MIB,
        value_parser = clap::value_parser!(u64).range(1..),
        hide = true
    )]
    pub block_cache_size_mib: u64,

    /// Bytes-per-sync threshold in `MiB`. Set 0 to disable.
    #[arg(
        long = "proofs-history.rocksdb.bytes-per-sync-mib",
        value_name = "PROOFS_HISTORY_ROCKSDB_BYTES_PER_SYNC_MIB",
        default_value_t = DEFAULT_ROCKSDB_BYTES_PER_SYNC_MIB,
        hide = true
    )]
    pub bytes_per_sync_mib: u64,

    /// Readahead size in `MiB` for compaction input reads. Set 0 to disable.
    #[arg(
        long = "proofs-history.rocksdb.compaction-readahead-size-mib",
        value_name = "PROOFS_HISTORY_ROCKSDB_COMPACTION_READAHEAD_SIZE_MIB",
        default_value_t = DEFAULT_ROCKSDB_COMPACTION_READAHEAD_SIZE_MIB,
        hide = true
    )]
    pub compaction_readahead_size_mib: u64,

    /// Number of L0 files that triggers compaction.
    #[arg(
        long = "proofs-history.rocksdb.level-zero-file-num-compaction-trigger",
        value_name = "PROOFS_HISTORY_ROCKSDB_LEVEL_ZERO_FILE_NUM_COMPACTION_TRIGGER",
        default_value_t = DEFAULT_ROCKSDB_LEVEL_ZERO_FILE_NUM_COMPACTION_TRIGGER,
        value_parser = clap::value_parser!(i32).range(1..),
        hide = true
    )]
    pub level_zero_file_num_compaction_trigger: i32,

    /// Number of L0 files that triggers write slowdown. Set 0 to disable slowdown.
    #[arg(
        long = "proofs-history.rocksdb.level-zero-slowdown-writes-trigger",
        value_name = "PROOFS_HISTORY_ROCKSDB_LEVEL_ZERO_SLOWDOWN_WRITES_TRIGGER",
        default_value_t = DEFAULT_ROCKSDB_LEVEL_ZERO_SLOWDOWN_WRITES_TRIGGER,
        value_parser = clap::value_parser!(i32).range(0..),
        hide = true
    )]
    pub level_zero_slowdown_writes_trigger: i32,

    /// Number of L0 files that stops writes.
    #[arg(
        long = "proofs-history.rocksdb.level-zero-stop-writes-trigger",
        value_name = "PROOFS_HISTORY_ROCKSDB_LEVEL_ZERO_STOP_WRITES_TRIGGER",
        default_value_t = DEFAULT_ROCKSDB_LEVEL_ZERO_STOP_WRITES_TRIGGER,
        value_parser = clap::value_parser!(i32).range(1..),
        hide = true
    )]
    pub level_zero_stop_writes_trigger: i32,

    /// Maximum `RocksDB` background jobs for proof history.
    #[arg(
        long = "proofs-history.rocksdb.max-background-jobs",
        value_name = "PROOFS_HISTORY_ROCKSDB_MAX_BACKGROUND_JOBS",
        default_value_t = DEFAULT_ROCKSDB_MAX_BACKGROUND_JOBS,
        value_parser = clap::value_parser!(i32).range(1..),
        hide = true
    )]
    pub max_background_jobs: i32,

    /// Maximum subcompactions per compaction.
    #[arg(
        long = "proofs-history.rocksdb.max-subcompactions",
        value_name = "PROOFS_HISTORY_ROCKSDB_MAX_SUBCOMPACTIONS",
        default_value_t = DEFAULT_ROCKSDB_MAX_SUBCOMPACTIONS,
        value_parser = clap::value_parser!(u32).range(1..),
        hide = true
    )]
    pub max_subcompactions: u32,

    /// Maximum total WAL size in `MiB`.
    #[arg(
        long = "proofs-history.rocksdb.max-total-wal-size-mib",
        value_name = "PROOFS_HISTORY_ROCKSDB_MAX_TOTAL_WAL_SIZE_MIB",
        value_parser = clap::value_parser!(u64).range(1..),
        hide = true
    )]
    pub max_total_wal_size_mib: Option<u64>,

    /// Maximum write buffers per proof-history column family.
    #[arg(
        long = "proofs-history.rocksdb.max-write-buffer-number",
        value_name = "PROOFS_HISTORY_ROCKSDB_MAX_WRITE_BUFFER_NUMBER",
        default_value_t = DEFAULT_ROCKSDB_MAX_WRITE_BUFFER_NUMBER,
        value_parser = clap::value_parser!(i32).range(1..),
        hide = true
    )]
    pub max_write_buffer_number: i32,

    /// Base target SST file size in `MiB`.
    #[arg(
        long = "proofs-history.rocksdb.target-file-size-base-mib",
        value_name = "PROOFS_HISTORY_ROCKSDB_TARGET_FILE_SIZE_BASE_MIB",
        default_value_t = DEFAULT_ROCKSDB_TARGET_FILE_SIZE_BASE_MIB,
        value_parser = clap::value_parser!(u64).range(1..),
        hide = true
    )]
    pub target_file_size_base_mib: u64,

    /// Write buffer size per proof-history column family in `MiB`.
    #[arg(
        long = "proofs-history.rocksdb.write-buffer-size-mib",
        value_name = "PROOFS_HISTORY_ROCKSDB_WRITE_BUFFER_SIZE_MIB",
        default_value_t = DEFAULT_ROCKSDB_WRITE_BUFFER_SIZE_MIB,
        value_parser = clap::value_parser!(u64).range(1..),
        hide = true
    )]
    pub write_buffer_size_mib: u64,

    /// Enable direct I/O for `RocksDB` flush and compaction.
    #[arg(
        long = "proofs-history.rocksdb.direct-io-for-flush-and-compaction",
        default_value_t = true,
        action = ArgAction::Set,
        hide = true
    )]
    pub direct_io_for_flush_and_compaction: bool,
}

impl Default for ProofsHistoryRocksdbArgs {
    fn default() -> Self {
        Self {
            block_cache_size_mib: DEFAULT_ROCKSDB_BLOCK_CACHE_SIZE_MIB,
            bytes_per_sync_mib: DEFAULT_ROCKSDB_BYTES_PER_SYNC_MIB,
            compaction_readahead_size_mib: DEFAULT_ROCKSDB_COMPACTION_READAHEAD_SIZE_MIB,
            level_zero_file_num_compaction_trigger:
                DEFAULT_ROCKSDB_LEVEL_ZERO_FILE_NUM_COMPACTION_TRIGGER,
            level_zero_slowdown_writes_trigger: DEFAULT_ROCKSDB_LEVEL_ZERO_SLOWDOWN_WRITES_TRIGGER,
            level_zero_stop_writes_trigger: DEFAULT_ROCKSDB_LEVEL_ZERO_STOP_WRITES_TRIGGER,
            max_background_jobs: DEFAULT_ROCKSDB_MAX_BACKGROUND_JOBS,
            max_subcompactions: DEFAULT_ROCKSDB_MAX_SUBCOMPACTIONS,
            max_total_wal_size_mib: None,
            max_write_buffer_number: DEFAULT_ROCKSDB_MAX_WRITE_BUFFER_NUMBER,
            target_file_size_base_mib: DEFAULT_ROCKSDB_TARGET_FILE_SIZE_BASE_MIB,
            write_buffer_size_mib: DEFAULT_ROCKSDB_WRITE_BUFFER_SIZE_MIB,
            direct_io_for_flush_and_compaction: true,
        }
    }
}

impl ProofsHistoryRocksdbArgs {
    /// Converts CLI arguments into storage options.
    pub fn storage_options(self) -> eyre::Result<RocksdbProofsStorageOptions> {
        if self.level_zero_slowdown_writes_trigger > 0
            && self.level_zero_file_num_compaction_trigger > self.level_zero_slowdown_writes_trigger
        {
            return Err(eyre::eyre!(
                "proofs-history.rocksdb.level-zero-file-num-compaction-trigger ({}) must be <= level-zero-slowdown-writes-trigger ({}) when slowdown is enabled",
                self.level_zero_file_num_compaction_trigger,
                self.level_zero_slowdown_writes_trigger
            ));
        }

        if self.level_zero_file_num_compaction_trigger > self.level_zero_stop_writes_trigger {
            return Err(eyre::eyre!(
                "proofs-history.rocksdb.level-zero-file-num-compaction-trigger ({}) must be <= level-zero-stop-writes-trigger ({})",
                self.level_zero_file_num_compaction_trigger,
                self.level_zero_stop_writes_trigger
            ));
        }

        Ok(RocksdbProofsStorageOptions {
            block_cache_size: mib_to_usize(self.block_cache_size_mib),
            bytes_per_sync: self.bytes_per_sync_mib.saturating_mul(MIB),
            compaction_readahead_size: mib_to_usize(self.compaction_readahead_size_mib),
            level_zero_file_num_compaction_trigger: self.level_zero_file_num_compaction_trigger,
            level_zero_slowdown_writes_trigger: self.level_zero_slowdown_writes_trigger,
            level_zero_stop_writes_trigger: self.level_zero_stop_writes_trigger,
            max_background_jobs: self.max_background_jobs,
            max_subcompactions: self.max_subcompactions,
            max_total_wal_size: self.max_total_wal_size_mib.map(|size| size.saturating_mul(MIB)),
            max_write_buffer_number: self.max_write_buffer_number,
            target_file_size_base: self.target_file_size_base_mib.saturating_mul(MIB),
            write_buffer_size: mib_to_usize(self.write_buffer_size_mib),
            use_direct_io_for_flush_and_compaction: self.direct_io_for_flush_and_compaction,
        })
    }
}

fn mib_to_usize(size_mib: u64) -> usize {
    usize::try_from(size_mib.saturating_mul(MIB)).unwrap_or(usize::MAX)
}

/// Parameters for rollup configuration
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
pub struct RollupArgs {
    /// Endpoint for the sequencer mempool (can be both HTTP and WS)
    #[arg(long = "rollup.sequencer", visible_aliases = ["rollup.sequencer-http", "rollup.sequencer-ws"])]
    pub sequencer: Option<String>,

    /// Disable transaction pool gossip
    #[arg(long = "rollup.disable-tx-pool-gossip")]
    pub disable_txpool_gossip: bool,

    /// By default the pending block equals the latest block
    /// to save resources and not leak txs from the tx-pool,
    /// this flag enables computing of the pending block
    /// from the tx-pool instead.
    ///
    /// If `compute_pending_block` is not enabled, the payload builder
    /// will use the payload attributes from the latest block. Note
    /// that this flag is not yet functional.
    #[arg(long = "rollup.compute-pending-block")]
    pub compute_pending_block: bool,

    /// enables discovery v4 if provided
    #[arg(long = "rollup.discovery.v4", default_value = "false")]
    pub discovery_v4: bool,

    /// Optional headers to use when connecting to the sequencer.
    #[arg(long = "rollup.sequencer-headers", requires = "sequencer")]
    pub sequencer_headers: Vec<String>,

    /// Minimum suggested priority fee (tip) in wei, default `1_000_000`
    #[arg(long, default_value_t = 1_000_000)]
    pub min_suggested_priority_fee: u64,

    /// Transaction ordering strategy for the mempool.
    ///
    /// Determines how transactions are prioritized when building blocks.
    /// - `coinbase-tip`: Order by priority fee (higher tip = higher priority). Default.
    /// - `timestamp`: Order by receive time (FIFO, earlier = higher priority).
    #[arg(long = "rollup.txpool-ordering", default_value = "coinbase-tip")]
    pub txpool_ordering: TxpoolOrdering,

    /// Maximum number of inflight EIP-7702 delegated account transactions per sender in the
    /// txpool. Reth defaults to 1, which prevents delegated accounts from submitting multiple
    /// transactions within a block (e.g. buy + approve in a single Flashblock).
    ///
    /// We raise the default to 4 (matching the EIP-8130 sender cap). Delegated code can move the
    /// account's balance mid-block, so a queued tx can become insolvent before the next canonical
    /// update; but that case fails fast — revm rejects on the pre-execution balance check before
    /// running any delegated code — so the only cost of a small cap is bounded (linear in the cap)
    /// mempool memory and cheap wasted pre-checks per account per block.
    #[arg(long = "rollup.txpool-max-inflight-delegated-slots", default_value_t = 4)]
    pub max_inflight_delegated_slots: usize,

    /// If true, initialize external-proofs exex to save and serve trie nodes to provide proofs
    /// faster.
    #[arg(
        long = "proofs-history",
        value_name = "PROOFS_HISTORY",
        default_value_ifs([
            ("proofs-history.storage-path", ArgPredicate::IsPresent, "true")
        ])
    )]
    pub proofs_history: bool,

    /// The path to the storage DB for proofs history.
    #[arg(long = "proofs-history.storage-path", value_name = "PROOFS_HISTORY_STORAGE_PATH")]
    pub proofs_history_storage_path: Option<PathBuf>,

    /// The on-disk database backend for proofs history.
    #[arg(long = "proofs-history.db", value_name = "PROOFS_HISTORY_DB", default_value = "mdbx")]
    pub proofs_history_db: ProofsHistoryDbBackend,

    /// Runtime tuning options for the `RocksDB` proofs history backend.
    #[command(flatten)]
    pub proofs_history_rocksdb: ProofsHistoryRocksdbArgs,

    /// Runtime tuning options for the `MDBX` proofs history backend.
    #[command(flatten)]
    pub proofs_history_mdbx: ProofsHistoryMdbxArgs,

    /// The window to span blocks for proofs history. Value is the number of blocks.
    /// Default is 1 month of blocks based on 2 seconds block time.
    /// 30 * 24 * 60 * 60 / 2 = `1_296_000`
    ///
    /// Must be greater than 12 hours of blocks based on 2 seconds block time.
    #[arg(
        long = "proofs-history.window",
        default_value_t = DEFAULT_PROOFS_HISTORY_WINDOW_BLOCKS,
        value_name = "PROOFS_HISTORY_WINDOW",
        value_parser = clap::value_parser!(u64).range((TWELVE_HOURS_IN_BLOCKS + 1)..)
    )]
    pub proofs_history_window: u64,

    /// Interval between proof-storage prune runs. Accepts human-friendly durations
    /// like "100s", "5m", "1h". Defaults to 15s.
    ///
    /// - Shorter intervals prune smaller batches more often, so each prune run tends to be faster
    ///   and the blocking pause for writes is shorter, at the cost of more frequent pauses.
    /// - Longer intervals prune larger batches less often, which reduces how often pruning runs,
    ///   but each run can take longer and block writes for longer.
    ///
    /// A shorter interval is preferred so that prune
    /// runs stay small and don’t stall writes for too long.
    ///
    /// CLI: `--proofs-history.prune-interval 10m`
    #[arg(
        long = "proofs-history.prune-interval",
        value_name = "PROOFS_HISTORY_PRUNE_INTERVAL",
        default_value = "15s",
        value_parser = humantime::parse_duration
    )]
    pub proofs_history_prune_interval: Duration,

    /// Verification interval: perform full block execution every N blocks for data integrity.
    /// - 0: Disabled (Default) (always use fast path with pre-computed data from notifications)
    /// - 1: Always verify (always execute blocks, slowest)
    /// - N: Verify every Nth block (e.g., 100 = every 100 blocks)
    ///
    /// Periodic verification helps catch data corruption or consensus bugs while maintaining
    /// good performance.
    ///
    /// CLI: `--proofs-history.verification-interval 100`
    #[arg(
        long = "proofs-history.verification-interval",
        value_name = "PROOFS_HISTORY_VERIFICATION_INTERVAL",
        default_value_t = 0
    )]
    pub proofs_history_verification_interval: u64,

    /// L1 upgrade signal observer arguments.
    #[command(flatten)]
    pub upgrade_signal: UpgradeSignalArgs,

    /// Execution-side L1 RPC argument for the upgrade signal observer.
    #[command(flatten)]
    pub upgrade_signal_l1_rpc: UpgradeSignalL1RpcArgs,
}

impl Default for RollupArgs {
    fn default() -> Self {
        Self {
            sequencer: None,
            disable_txpool_gossip: false,
            compute_pending_block: false,
            discovery_v4: false,
            sequencer_headers: Vec::new(),
            min_suggested_priority_fee: 1_000_000,
            txpool_ordering: TxpoolOrdering::default(),
            max_inflight_delegated_slots: 4,
            proofs_history: false,
            proofs_history_storage_path: None,
            proofs_history_db: ProofsHistoryDbBackend::default(),
            proofs_history_rocksdb: Default::default(),
            proofs_history_mdbx: Default::default(),
            proofs_history_window: DEFAULT_PROOFS_HISTORY_WINDOW_BLOCKS,
            proofs_history_prune_interval: Duration::from_secs(15),
            proofs_history_verification_interval: 0,
            upgrade_signal: UpgradeSignalArgs::default(),
            upgrade_signal_l1_rpc: UpgradeSignalL1RpcArgs::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use clap::{Args, CommandFactory, Parser};

    use super::*;

    /// A helper type to parse Args more easily
    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    #[test]
    fn test_parse_rollup_default_args() {
        let default_args = RollupArgs::default();
        let args = CommandParser::<RollupArgs>::parse_from(["reth"]).args;
        assert_eq!(args, default_args);
    }

    #[test]
    fn test_parse_rollup_compute_pending_block_args() {
        let expected_args = RollupArgs { compute_pending_block: true, ..Default::default() };
        let args =
            CommandParser::<RollupArgs>::parse_from(["reth", "--rollup.compute-pending-block"])
                .args;
        assert_eq!(args, expected_args);
    }

    #[test]
    fn test_parse_rollup_discovery_v4_args() {
        let expected_args = RollupArgs { discovery_v4: true, ..Default::default() };
        let args = CommandParser::<RollupArgs>::parse_from(["reth", "--rollup.discovery.v4"]).args;
        assert_eq!(args, expected_args);
    }

    #[test]
    fn test_parse_rollup_sequencer_http_args() {
        let expected_args =
            RollupArgs { sequencer: Some("http://host:port".into()), ..Default::default() };
        let args = CommandParser::<RollupArgs>::parse_from([
            "reth",
            "--rollup.sequencer-http",
            "http://host:port",
        ])
        .args;
        assert_eq!(args, expected_args);
    }

    #[test]
    fn test_parse_rollup_disable_txpool_args() {
        let expected_args = RollupArgs { disable_txpool_gossip: true, ..Default::default() };
        let args =
            CommandParser::<RollupArgs>::parse_from(["reth", "--rollup.disable-tx-pool-gossip"])
                .args;
        assert_eq!(args, expected_args);
    }

    #[test]
    fn test_parse_rollup_many_args() {
        let expected_args = RollupArgs {
            disable_txpool_gossip: true,
            compute_pending_block: true,
            sequencer: Some("http://host:port".into()),
            ..Default::default()
        };
        let args = CommandParser::<RollupArgs>::parse_from([
            "reth",
            "--rollup.disable-tx-pool-gossip",
            "--rollup.compute-pending-block",
            "--rollup.sequencer-http",
            "http://host:port",
        ])
        .args;
        assert_eq!(args, expected_args);
    }

    #[test]
    fn test_parse_max_inflight_delegated_slots_default() {
        let args = CommandParser::<RollupArgs>::parse_from(["reth"]).args;
        assert_eq!(args.max_inflight_delegated_slots, 4);
        assert_eq!(
            args.max_inflight_delegated_slots,
            RollupArgs::default().max_inflight_delegated_slots
        );
    }

    #[test]
    fn test_parse_max_inflight_delegated_slots_override() {
        let expected_args = RollupArgs { max_inflight_delegated_slots: 7, ..Default::default() };
        let args = CommandParser::<RollupArgs>::parse_from([
            "reth",
            "--rollup.txpool-max-inflight-delegated-slots",
            "7",
        ])
        .args;
        assert_eq!(args, expected_args);
    }

    #[test]
    fn test_parse_txpool_ordering_default() {
        let args = CommandParser::<RollupArgs>::parse_from(["reth"]).args;
        assert_eq!(args.txpool_ordering, TxpoolOrdering::CoinbaseTip);
    }

    #[test]
    fn test_parse_txpool_ordering_coinbase_tip() {
        let args = CommandParser::<RollupArgs>::parse_from([
            "reth",
            "--rollup.txpool-ordering",
            "coinbase-tip",
        ])
        .args;
        assert_eq!(args.txpool_ordering, TxpoolOrdering::CoinbaseTip);
    }

    #[test]
    fn test_parse_txpool_ordering_timestamp() {
        let args = CommandParser::<RollupArgs>::parse_from([
            "reth",
            "--rollup.txpool-ordering",
            "timestamp",
        ])
        .args;
        assert_eq!(args.txpool_ordering, TxpoolOrdering::Timestamp);
    }

    #[test]
    fn test_parse_upgrade_signal_args() {
        let contract = address!("0000000000000000000000000000000000000001");
        let args = CommandParser::<RollupArgs>::parse_from([
            "reth",
            "--upgrade-signal.contract",
            "0x0000000000000000000000000000000000000001",
            "--upgrade-signal.upgrade-id",
            "azul",
            "--upgrade-signal.l1-rpc",
            "http://localhost:8545",
        ])
        .args;

        assert_eq!(args.upgrade_signal.contract_address, Some(contract));
        assert_eq!(args.upgrade_signal.upgrade_ids, ["azul"]);
        assert_eq!(
            args.upgrade_signal_l1_rpc.upgrade_signal_l1_rpc.as_ref().map(|url| url.as_str()),
            Some("http://localhost:8545/")
        );
    }

    #[test]
    fn test_parse_proofs_history_db_default() {
        let args = CommandParser::<RollupArgs>::parse_from(["reth"]).args;
        assert_eq!(args.proofs_history_db, ProofsHistoryDbBackend::Mdbx);
    }

    #[test]
    fn test_parse_proofs_history_db_v2_alias() {
        let args =
            CommandParser::<RollupArgs>::parse_from(["reth", "--proofs-history.db", "v2"]).args;
        assert_eq!(args.proofs_history_db, ProofsHistoryDbBackend::Rocksdb);
    }

    #[test]
    fn test_parse_proofs_history_db_mdbx() {
        let args = CommandParser::<RollupArgs>::parse_from([
            "reth",
            "--proofs-history",
            "--proofs-history.db",
            "mdbx",
        ])
        .args;
        assert!(args.proofs_history);
        assert_eq!(args.proofs_history_db, ProofsHistoryDbBackend::Mdbx);
    }

    #[test]
    fn test_parse_proofs_history_db_without_enabling_history() {
        let args =
            CommandParser::<RollupArgs>::parse_from(["reth", "--proofs-history.db", "mdbx"]).args;
        assert!(!args.proofs_history);
        assert_eq!(args.proofs_history_db, ProofsHistoryDbBackend::Mdbx);
    }

    #[test]
    fn test_parse_proofs_history_mdbx_tuning_options() {
        let args = CommandParser::<RollupArgs>::parse_from([
            "reth",
            "--proofs-history.mdbx.max-read-transaction-duration",
            "30s",
        ])
        .args;

        let options = args.proofs_history_mdbx.storage_options();
        assert_eq!(options.max_read_transaction_duration, Some(Duration::from_secs(30)));
    }

    #[test]
    fn test_proofs_history_mdbx_rejects_zero_duration() {
        let result = CommandParser::<RollupArgs>::try_parse_from([
            "reth",
            "--proofs-history.mdbx.max-read-transaction-duration",
            "0s",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn test_proofs_history_db_rejects_ambiguous_storage_markers() {
        let dir = std::env::temp_dir().join(format!(
            "proofs-history-markers-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("CURRENT"), b"rocksdb").unwrap();
        std::fs::write(dir.join("mdbx.dat"), b"mdbx").unwrap();

        let rocksdb_error =
            ProofsHistoryDbBackend::Rocksdb.ensure_storage_path_matches(&dir).unwrap_err();
        assert!(
            rocksdb_error
                .to_string()
                .contains("both RocksDB marker CURRENT and MDBX marker mdbx.dat")
        );

        let mdbx_error =
            ProofsHistoryDbBackend::Mdbx.ensure_storage_path_matches(&dir).unwrap_err();
        assert!(
            mdbx_error.to_string().contains("both RocksDB marker CURRENT and MDBX marker mdbx.dat")
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_parse_proofs_history_rocksdb_tuning_options() {
        let args = CommandParser::<RollupArgs>::parse_from([
            "reth",
            "--proofs-history.rocksdb.block-cache-size-mib",
            "256",
            "--proofs-history.rocksdb.bytes-per-sync-mib",
            "4",
            "--proofs-history.rocksdb.compaction-readahead-size-mib",
            "8",
            "--proofs-history.rocksdb.level-zero-file-num-compaction-trigger",
            "6",
            "--proofs-history.rocksdb.level-zero-slowdown-writes-trigger",
            "0",
            "--proofs-history.rocksdb.level-zero-stop-writes-trigger",
            "64",
            "--proofs-history.rocksdb.max-background-jobs",
            "2",
            "--proofs-history.rocksdb.max-subcompactions",
            "2",
            "--proofs-history.rocksdb.max-total-wal-size-mib",
            "1024",
            "--proofs-history.rocksdb.max-write-buffer-number",
            "4",
            "--proofs-history.rocksdb.target-file-size-base-mib",
            "512",
            "--proofs-history.rocksdb.write-buffer-size-mib",
            "128",
            "--proofs-history.rocksdb.direct-io-for-flush-and-compaction",
            "false",
        ])
        .args;

        let options = args.proofs_history_rocksdb.storage_options().unwrap();
        assert_eq!(options.block_cache_size, mib_to_usize(256));
        assert_eq!(options.bytes_per_sync, 4 * MIB);
        assert_eq!(options.compaction_readahead_size, mib_to_usize(8));
        assert_eq!(options.level_zero_file_num_compaction_trigger, 6);
        assert_eq!(options.level_zero_slowdown_writes_trigger, 0);
        assert_eq!(options.level_zero_stop_writes_trigger, 64);
        assert_eq!(options.max_background_jobs, 2);
        assert_eq!(options.max_subcompactions, 2);
        assert_eq!(options.max_total_wal_size, Some(1024 * MIB));
        assert_eq!(options.max_write_buffer_number, 4);
        assert_eq!(options.target_file_size_base, 512 * MIB);
        assert_eq!(options.write_buffer_size, mib_to_usize(128));
        assert!(!options.use_direct_io_for_flush_and_compaction);
    }

    #[test]
    fn test_proofs_history_rocksdb_tuning_options_hidden_from_help() {
        let help = CommandParser::<RollupArgs>::command().render_help().to_string();
        assert!(!help.contains("proofs-history.mdbx.max-read-transaction-duration"));
        assert!(!help.contains("proofs-history.rocksdb.compression"));
        assert!(!help.contains("proofs-history.rocksdb.max-background-jobs"));
        assert!(!help.contains("proofs-history.rocksdb.rate-limit-mib-per-sec"));
    }

    #[test]
    fn test_parse_proofs_history_rocksdb_tuning_rejects_bad_l0_ordering() {
        let args = CommandParser::<RollupArgs>::parse_from([
            "reth",
            "--proofs-history.rocksdb.level-zero-file-num-compaction-trigger",
            "40",
            "--proofs-history.rocksdb.level-zero-stop-writes-trigger",
            "1",
        ])
        .args;

        assert!(args.proofs_history_rocksdb.storage_options().is_err());
    }

    #[test]
    fn test_parse_proofs_history_window() {
        let args =
            CommandParser::<RollupArgs>::parse_from(["reth", "--proofs-history.window", "21601"])
                .args;
        assert_eq!(args.proofs_history_window, 21_601);
    }

    #[test]
    fn test_parse_proofs_history_window_rejects_twelve_hours_or_less() {
        let result = CommandParser::<RollupArgs>::try_parse_from([
            "reth",
            "--proofs-history.window",
            "21600",
        ]);
        assert!(result.is_err());
    }
}
