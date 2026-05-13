//! Write-path benchmarks for proofs storage backends.
//!
//! This is intentionally not a Criterion benchmark: the production question is
//! about sustained block write latency after a large preload, so the benchmark
//! runs one long workload and prints Datadog-like quantiles.

use std::{
    cmp::Ordering,
    env,
    fs::{self, DirEntry},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
use alloy_primitives::{B256, U256};
use base_execution_trie::{
    BaseProofsInitialStateStore, BaseProofsStore, BlockStateDiff, MdbxProofsStorage,
    RocksdbProofsStorage,
    db::{
        HashedStorageHistory, HashedStorageKey, MaybeDeleted, StorageValue, Tables, VersionedValue,
    },
};
use eyre::{Result, WrapErr, bail, eyre};
use reth_db::{
    Database,
    cursor::{DbCursorRW, DbDupCursorRO, DbDupCursorRW},
    mdbx::{DatabaseArguments, init_db_for},
    table::{Compress, Encode},
    transaction::{DbTx, DbTxMut},
};
use reth_trie::hashed_cursor::HashedCursor;
use reth_trie_common::{HashedPostState, HashedStorage};
use rocksdb::{
    ColumnFamilyDescriptor, DBCompressionType, DBWithThreadMode, MultiThreaded, Options,
    WriteBatch, WriteOptions,
};
use tempfile::TempDir;

const HASHED_STORAGE_HISTORY_CF: &str = "hashed_storage_history";
const DEFAULT_PRELOAD_ROWS: u64 = 10_000_000;
const DEFAULT_WRITES_PER_BLOCK: usize = 20_000;
const DEFAULT_MEASURED_BLOCKS: u64 = 1_000;
const DEFAULT_ACCOUNTS: u64 = 100_000;
const DEFAULT_READER_THREADS: usize = 4;
const DEFAULT_PRUNE_BATCH_BLOCKS: u64 = 1;
const DEFAULT_PRUNE_WINDOW_BLOCKS: u64 = 256;
const DEFAULT_SEED: u64 = 0x5eed;

fn main() -> Result<()> {
    let config = Config::parse()?;
    config.print();

    for backend in config.backends.backends() {
        run_backend(&config, backend)?;
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendSelection {
    All,
    ApiMdbx,
    ApiRocksdb,
    RawMdbx,
    RawRocksdb,
}

impl BackendSelection {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "all" => Ok(Self::All),
            "api-mdbx" => Ok(Self::ApiMdbx),
            "api-rocksdb" => Ok(Self::ApiRocksdb),
            "raw-mdbx" => Ok(Self::RawMdbx),
            "raw-rocksdb" => Ok(Self::RawRocksdb),
            other => {
                bail!(
                    "unknown backend {other:?}; expected all, api-mdbx, api-rocksdb, raw-mdbx, or raw-rocksdb"
                )
            }
        }
    }

    fn backends(self) -> Vec<BackendKind> {
        match self {
            Self::All => vec![
                BackendKind::RawMdbx,
                BackendKind::RawRocksdb,
                BackendKind::ApiMdbx,
                BackendKind::ApiRocksdb,
            ],
            Self::ApiMdbx => vec![BackendKind::ApiMdbx],
            Self::ApiRocksdb => vec![BackendKind::ApiRocksdb],
            Self::RawMdbx => vec![BackendKind::RawMdbx],
            Self::RawRocksdb => vec![BackendKind::RawRocksdb],
        }
    }

    const fn includes_api_backend(self) -> bool {
        matches!(self, Self::All | Self::ApiMdbx | Self::ApiRocksdb)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    ApiMdbx,
    ApiRocksdb,
    RawMdbx,
    RawRocksdb,
}

impl BackendKind {
    const fn name(self) -> &'static str {
        match self {
            Self::ApiMdbx => "api-mdbx",
            Self::ApiRocksdb => "api-rocksdb",
            Self::RawMdbx => "raw-mdbx",
            Self::RawRocksdb => "raw-rocksdb",
        }
    }
}

#[derive(Debug, Clone)]
struct Config {
    accounts: u64,
    background_writers: usize,
    backends: BackendSelection,
    keep_dirs: bool,
    measured_blocks: u64,
    prune_batch_blocks: u64,
    prune_window_blocks: u64,
    pruner: bool,
    preload_rows: u64,
    reader_threads: usize,
    rocksdb_sync: bool,
    seed: u64,
    writes_per_block: usize,
}

impl Config {
    fn parse() -> Result<Self> {
        let mut config = Self {
            accounts: DEFAULT_ACCOUNTS,
            background_writers: 0,
            backends: BackendSelection::All,
            keep_dirs: false,
            measured_blocks: DEFAULT_MEASURED_BLOCKS,
            prune_batch_blocks: DEFAULT_PRUNE_BATCH_BLOCKS,
            prune_window_blocks: DEFAULT_PRUNE_WINDOW_BLOCKS,
            pruner: false,
            preload_rows: DEFAULT_PRELOAD_ROWS,
            reader_threads: DEFAULT_READER_THREADS,
            rocksdb_sync: true,
            seed: DEFAULT_SEED,
            writes_per_block: DEFAULT_WRITES_PER_BLOCK,
        };

        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--accounts" => {
                    config.accounts = parse_next(&mut args, "--accounts")?;
                }
                "--backend" => {
                    let backend = next_value(&mut args, "--backend")?;
                    config.backends = BackendSelection::parse(&backend)?;
                }
                "--bench" => {}
                "--background-writers" | "--concurrent-writers" | "--writer-threads" => {
                    config.background_writers = parse_next(&mut args, "--background-writers")?;
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                "--keep-dirs" => {
                    config.keep_dirs = true;
                }
                "--measured-blocks" => {
                    config.measured_blocks = parse_next(&mut args, "--measured-blocks")?;
                }
                "--preload-rows" | "--rows" => {
                    config.preload_rows = parse_next(&mut args, "--preload-rows")?;
                }
                "--prune-batch-blocks" => {
                    config.prune_batch_blocks = parse_next(&mut args, "--prune-batch-blocks")?;
                }
                "--prune-window-blocks" => {
                    config.prune_window_blocks = parse_next(&mut args, "--prune-window-blocks")?;
                }
                "--pruner" | "--background-pruner" => {
                    config.pruner = true;
                }
                "--reader-threads" | "--readers" => {
                    config.reader_threads = parse_next(&mut args, "--reader-threads")?;
                }
                "--rocksdb-no-sync" => {
                    config.rocksdb_sync = false;
                }
                "--seed" => {
                    config.seed = parse_seed(&next_value(&mut args, "--seed")?)?;
                }
                "--writes-per-block" | "--batch-size" => {
                    config.writes_per_block = parse_next(&mut args, "--writes-per-block")?;
                }
                other => bail!("unknown argument {other:?}; pass --help for usage"),
            }
        }

        if config.accounts == 0 {
            bail!("--accounts must be greater than zero");
        }
        if config.writes_per_block == 0 {
            bail!("--writes-per-block must be greater than zero");
        }
        if config.preload_rows == 0 {
            bail!("--preload-rows must be greater than zero");
        }
        if !config.preload_rows.is_multiple_of(config.writes_per_block as u64) {
            bail!("--preload-rows must be a multiple of --writes-per-block");
        }
        if config.prune_batch_blocks == 0 {
            bail!("--prune-batch-blocks must be greater than zero");
        }
        if config.prune_window_blocks == 0 {
            bail!("--prune-window-blocks must be greater than zero");
        }
        if (config.background_writers > 0 || config.pruner)
            && config.backends.includes_api_backend()
        {
            bail!(
                "--background-writers and --pruner are only supported by raw-mdbx and raw-rocksdb; \
                 use --backend raw-mdbx or --backend raw-rocksdb"
            );
        }

        Ok(config)
    }

    fn print(&self) {
        println!("proofs_storage_write benchmark");
        println!("  preload_rows       {}", self.preload_rows);
        println!("  measured_blocks    {}", self.measured_blocks);
        println!("  writes_per_block   {}", self.writes_per_block);
        println!("  accounts           {}", self.accounts);
        println!("  reader_threads     {}", self.reader_threads);
        println!("  background_writers {}", self.background_writers);
        println!("  pruner             {}", self.pruner);
        println!("  prune_window_blocks {}", self.prune_window_blocks);
        println!("  prune_batch_blocks {}", self.prune_batch_blocks);
        println!("  rocksdb_sync       {}", self.rocksdb_sync);
        println!("  seed               {:#x}", self.seed);
        println!("  keep_dirs          {}", self.keep_dirs);
        println!();
    }
}

fn print_help() {
    println!(
        "usage: cargo bench -p base-execution-trie --bench proofs_storage_write -- [options]\n\
\n\
options:\n\
  --backend <all|raw-mdbx|raw-rocksdb|api-mdbx|api-rocksdb>\n\
  --preload-rows <n>       rows inserted before measurement [default: 10000000]\n\
  --measured-blocks <n>    block-sized write commits during measurement [default: 1000]\n\
  --writes-per-block <n>   random storage writes per measured block [default: 20000]\n\
  --accounts <n>           distinct hashed accounts in the random keyspace [default: 100000]\n\
  --reader-threads <n>     concurrent random read threads during measurement [default: 4]\n\
  --background-writers <n> optional extra raw write loops for stress testing [default: 0]\n\
  --pruner                 run one background raw prune loop during measurement\n\
  --prune-window-blocks <n> keep this many newest committed blocks [default: 256]\n\
  --prune-batch-blocks <n> blocks deleted per prune transaction [default: 1]\n\
  --rocksdb-no-sync        use RocksDB WAL without per-block sync\n\
  --seed <n|0xhex>         deterministic workload seed [default: 0x5eed]\n\
  --keep-dirs              keep database directories after the run\n"
    );
}

fn next_value(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next().ok_or_else(|| eyre!("{name} requires a value"))
}

fn parse_next<T>(args: &mut impl Iterator<Item = String>, name: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    next_value(args, name)?.parse::<T>().wrap_err_with(|| format!("invalid value for {name}"))
}

fn parse_seed(value: &str) -> Result<u64> {
    if let Some(hex) = value.strip_prefix("0x") {
        return u64::from_str_radix(hex, 16).wrap_err("invalid --seed");
    }
    value.parse::<u64>().wrap_err("invalid --seed")
}

trait BenchBackend: Send + Sync {
    fn name(&self) -> &'static str;

    fn write_block(&self, rows: &[ProofRow]) -> Result<()>;

    fn read_existing(&self, row: &ProofRow) -> Result<bool>;

    fn prune_block(&self, _rows: &[ProofRow]) -> Result<PruneCounts> {
        bail!("{} does not support raw prune simulation", self.name())
    }

    fn finish(&self) -> Result<()> {
        Ok(())
    }

    fn stats(&self) -> Result<Vec<(String, String)>> {
        Ok(Vec::new())
    }
}

#[derive(Debug, Clone, Copy)]
struct ProofRow {
    address: B256,
    slot: B256,
    value: U256,
    block_number: u64,
}

impl Ord for ProofRow {
    fn cmp(&self, other: &Self) -> Ordering {
        self.address.cmp(&other.address).then_with(|| self.slot.cmp(&other.slot))
    }
}

impl PartialOrd for ProofRow {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ProofRow {
    fn eq(&self, other: &Self) -> bool {
        self.address == other.address && self.slot == other.slot
    }
}

impl Eq for ProofRow {}

fn run_backend(config: &Config, kind: BackendKind) -> Result<()> {
    let temp_dir = TempDir::with_prefix(format!("proofs-storage-write-{}-", kind.name()).as_str())?;
    let path = temp_dir.path().to_path_buf();
    let backend = open_backend(config, kind, &path)?;

    println!("backend {}", backend.name());
    println!("  path {}", path.display());

    let preload = preload_rows(config, &backend)?;
    print_summary("  preload", config.preload_rows, &preload, None, None, None);

    let measured = run_measured(config, Arc::clone(&backend))?;
    print_summary(
        "  measured_writes",
        config.measured_blocks * config.writes_per_block as u64,
        &measured.write_latencies,
        Some(measured.reader_stats),
        Some(measured.background_writer_stats),
        Some(measured.pruner_stats),
    );

    backend.finish()?;

    for (key, value) in backend.stats()? {
        println!("  {key:<28} {value}");
    }

    let sizes = directory_sizes(&path)?;
    println!("  directory_apparent_bytes   {}", sizes.apparent_bytes);
    println!("  directory_allocated_bytes  {}", sizes.allocated_bytes);

    if config.keep_dirs {
        let kept = temp_dir.keep();
        println!("  kept_path                  {}", kept.display());
    }

    println!();
    Ok(())
}

fn open_backend(config: &Config, kind: BackendKind, path: &Path) -> Result<Arc<dyn BenchBackend>> {
    match kind {
        BackendKind::ApiMdbx => Ok(Arc::new(ApiMdbxBackend::open(path)?)),
        BackendKind::ApiRocksdb => Ok(Arc::new(ApiRocksdbBackend::open(path)?)),
        BackendKind::RawMdbx => Ok(Arc::new(RawMdbxBackend::open(path)?)),
        BackendKind::RawRocksdb => Ok(Arc::new(RawRocksdbBackend::open(path, config)?)),
    }
}

fn preload_rows(config: &Config, backend: &Arc<dyn BenchBackend>) -> Result<Vec<Duration>> {
    let mut latencies = Vec::with_capacity(blocks_for_rows(config.preload_rows, config) as usize);
    let mut row_index = 0;

    while row_index < config.preload_rows {
        let remaining = config.preload_rows - row_index;
        let rows_in_block = remaining.min(config.writes_per_block as u64) as usize;
        let block_number = block_number_for_row(row_index, config);
        let rows = generate_block(config, row_index, rows_in_block);
        let start = Instant::now();
        backend.write_block(&rows)?;
        latencies.push(start.elapsed());
        row_index += rows_in_block as u64;

        if latencies.len() % 100 == 0 {
            println!("  preloaded_blocks           {} ({row_index} rows)", latencies.len());
        }

        if rows.first().map(|row| row.block_number) != Some(block_number) {
            bail!("generated rows crossed a block boundary");
        }
    }

    Ok(latencies)
}

fn run_measured(config: &Config, backend: Arc<dyn BenchBackend>) -> Result<MeasuredOutput> {
    let committed_rows = Arc::new(AtomicU64::new(config.preload_rows));
    let stop = Arc::new(AtomicBool::new(false));
    let reader_handles =
        spawn_readers(config, Arc::clone(&backend), Arc::clone(&committed_rows), Arc::clone(&stop));
    let pruner_handle =
        spawn_pruner(config, Arc::clone(&backend), Arc::clone(&committed_rows), Arc::clone(&stop));
    let background_writer_handles =
        spawn_background_writers(config, Arc::clone(&backend), Arc::clone(&stop));
    let mut write_latencies = Vec::with_capacity(config.measured_blocks as usize);

    let mut next_row_index = config.preload_rows;
    for measured_block in 0..config.measured_blocks {
        let rows = generate_block(config, next_row_index, config.writes_per_block);
        let start = Instant::now();
        backend.write_block(&rows)?;
        write_latencies.push(start.elapsed());
        next_row_index += config.writes_per_block as u64;
        committed_rows.store(next_row_index, AtomicOrdering::Release);

        if (measured_block + 1) % 100 == 0 {
            println!(
                "  measured_blocks_written    {} / {}",
                measured_block + 1,
                config.measured_blocks
            );
        }
    }

    stop.store(true, AtomicOrdering::Release);

    let mut reader_stats = ReaderStats::default();
    for handle in reader_handles {
        reader_stats += handle.join().map_err(|_| eyre!("reader thread panicked"))??;
    }

    let mut background_writer_stats = WriterStats::default();
    for handle in background_writer_handles {
        background_writer_stats +=
            handle.join().map_err(|_| eyre!("background writer thread panicked"))??;
    }

    let pruner_stats = if let Some(handle) = pruner_handle {
        handle.join().map_err(|_| eyre!("pruner thread panicked"))??
    } else {
        PrunerStats::default()
    };

    Ok(MeasuredOutput { background_writer_stats, pruner_stats, reader_stats, write_latencies })
}

fn spawn_readers(
    config: &Config,
    backend: Arc<dyn BenchBackend>,
    committed_rows: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) -> Vec<JoinHandle<Result<ReaderStats>>> {
    (0..config.reader_threads)
        .map(|reader_index| {
            let backend = Arc::clone(&backend);
            let committed_rows = Arc::clone(&committed_rows);
            let stop = Arc::clone(&stop);
            let config = config.clone();
            thread::spawn(move || {
                let mut rng =
                    SplitMix64::new(config.seed ^ 0xd1b5_4a32_d192_ed03 ^ reader_index as u64);
                let start = Instant::now();
                let mut stats = ReaderStats::default();

                while !stop.load(AtomicOrdering::Acquire) {
                    let committed = committed_rows.load(AtomicOrdering::Acquire);
                    if committed == 0 {
                        thread::yield_now();
                        continue;
                    }

                    let oldest_readable = oldest_readable_row(&config, committed);
                    let row_index = oldest_readable + rng.next_bounded(committed - oldest_readable);
                    let row = row_for_index(&config, row_index);
                    if backend.read_existing(&row)? {
                        stats.hits += 1;
                    } else {
                        stats.misses += 1;
                    }
                    stats.operations += 1;
                }

                stats.duration = start.elapsed();
                Ok(stats)
            })
        })
        .collect()
}

fn spawn_pruner(
    config: &Config,
    backend: Arc<dyn BenchBackend>,
    committed_rows: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) -> Option<JoinHandle<Result<PrunerStats>>> {
    if !config.pruner {
        return None;
    }

    let config = config.clone();
    Some(thread::spawn(move || {
        let start = Instant::now();
        let mut next_prune_block = 1;
        let mut stats = PrunerStats::default();

        while !stop.load(AtomicOrdering::Acquire) {
            let committed = committed_rows.load(AtomicOrdering::Acquire);
            let committed_block = committed / config.writes_per_block as u64;
            let eligible_block = committed_block.saturating_sub(config.prune_window_blocks);

            if next_prune_block > eligible_block {
                thread::sleep(Duration::from_millis(10));
                continue;
            }

            let batch_end = (next_prune_block + config.prune_batch_blocks - 1).min(eligible_block);
            let prune_start = Instant::now();
            let mut counts = PruneCounts::default();

            for block_number in next_prune_block..=batch_end {
                let row_index = (block_number - 1) * config.writes_per_block as u64;
                let rows = generate_block(&config, row_index, config.writes_per_block);
                counts += backend.prune_block(&rows)?;
                stats.blocks += 1;
            }

            stats.latencies.push(prune_start.elapsed());
            stats.rows_scanned += counts.scanned;
            stats.rows_found += counts.found;
            stats.rows_deleted += counts.deleted;
            next_prune_block = batch_end + 1;
        }

        stats.duration = start.elapsed();
        Ok(stats)
    }))
}

fn spawn_background_writers(
    config: &Config,
    backend: Arc<dyn BenchBackend>,
    stop: Arc<AtomicBool>,
) -> Vec<JoinHandle<Result<WriterStats>>> {
    let measured_rows = config.measured_blocks * config.writes_per_block as u64;
    let first_background_row = config.preload_rows + measured_rows;

    (0..config.background_writers)
        .map(|writer_index| {
            let backend = Arc::clone(&backend);
            let stop = Arc::clone(&stop);
            let config = config.clone();
            thread::spawn(move || {
                let mut stats = WriterStats::default();
                let mut block_offset = writer_index as u64;
                let block_stride = config.background_writers as u64;
                let start = Instant::now();

                while !stop.load(AtomicOrdering::Acquire) {
                    let row_index =
                        first_background_row + block_offset * config.writes_per_block as u64;
                    let rows = generate_block(&config, row_index, config.writes_per_block);
                    let write_start = Instant::now();
                    backend.write_block(&rows)?;
                    stats.latencies.push(write_start.elapsed());
                    stats.blocks += 1;
                    stats.rows += config.writes_per_block as u64;
                    block_offset += block_stride;
                }

                stats.duration = start.elapsed();
                Ok(stats)
            })
        })
        .collect()
}

#[derive(Debug)]
struct MeasuredOutput {
    background_writer_stats: WriterStats,
    pruner_stats: PrunerStats,
    reader_stats: ReaderStats,
    write_latencies: Vec<Duration>,
}

#[derive(Debug, Clone, Copy, Default)]
struct PruneCounts {
    deleted: u64,
    found: u64,
    scanned: u64,
}

impl std::ops::AddAssign for PruneCounts {
    fn add_assign(&mut self, rhs: Self) {
        self.deleted += rhs.deleted;
        self.found += rhs.found;
        self.scanned += rhs.scanned;
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ReaderStats {
    duration: Duration,
    hits: u64,
    misses: u64,
    operations: u64,
}

impl std::ops::AddAssign for ReaderStats {
    fn add_assign(&mut self, rhs: Self) {
        self.duration = self.duration.max(rhs.duration);
        self.hits += rhs.hits;
        self.misses += rhs.misses;
        self.operations += rhs.operations;
    }
}

#[derive(Debug, Default)]
struct WriterStats {
    blocks: u64,
    duration: Duration,
    latencies: Vec<Duration>,
    rows: u64,
}

impl std::ops::AddAssign for WriterStats {
    fn add_assign(&mut self, rhs: Self) {
        self.blocks += rhs.blocks;
        self.duration = self.duration.max(rhs.duration);
        self.latencies.extend(rhs.latencies);
        self.rows += rhs.rows;
    }
}

#[derive(Debug, Default)]
struct PrunerStats {
    blocks: u64,
    duration: Duration,
    latencies: Vec<Duration>,
    rows_deleted: u64,
    rows_found: u64,
    rows_scanned: u64,
}

struct RawMdbxBackend {
    env: reth_db::DatabaseEnv,
}

impl RawMdbxBackend {
    fn open(path: &Path) -> Result<Self> {
        let env = init_db_for::<_, Tables>(path, DatabaseArguments::default())
            .wrap_err("failed to open raw MDBX database")?;
        Ok(Self { env })
    }
}

impl BenchBackend for RawMdbxBackend {
    fn name(&self) -> &'static str {
        "raw-mdbx"
    }

    fn write_block(&self, rows: &[ProofRow]) -> Result<()> {
        let tx = self.env.tx_mut()?;
        let mut cursor = tx.cursor_dup_write::<HashedStorageHistory>()?;

        for row in rows {
            cursor.append_dup(
                HashedStorageKey::new(row.address, row.slot),
                VersionedValue::new(
                    row.block_number,
                    MaybeDeleted(Some(StorageValue::new(row.value))),
                ),
            )?;
        }

        drop(cursor);
        tx.commit()?;
        Ok(())
    }

    fn read_existing(&self, row: &ProofRow) -> Result<bool> {
        let tx = self.env.tx()?;
        let mut cursor = tx.cursor_dup_read::<HashedStorageHistory>()?;
        let found = cursor
            .seek_by_key_subkey(HashedStorageKey::new(row.address, row.slot), row.block_number)?
            .is_some_and(|value| value.block_number == row.block_number);
        tx.commit()?;
        Ok(found)
    }

    fn prune_block(&self, rows: &[ProofRow]) -> Result<PruneCounts> {
        let tx = self.env.tx_mut()?;
        let mut cursor = tx.cursor_dup_write::<HashedStorageHistory>()?;
        let mut counts = PruneCounts::default();

        for row in rows {
            counts.scanned += 1;
            if let Some(value) = cursor.seek_by_key_subkey(
                HashedStorageKey::new(row.address, row.slot),
                row.block_number,
            )? && value.block_number == row.block_number
            {
                counts.found += 1;
                cursor.delete_current()?;
                counts.deleted += 1;
            }
        }

        drop(cursor);
        tx.commit()?;
        Ok(counts)
    }
}

struct ApiMdbxBackend {
    storage: MdbxProofsStorage,
}

impl ApiMdbxBackend {
    fn open(path: &Path) -> Result<Self> {
        let storage = MdbxProofsStorage::new(path).wrap_err("failed to open API MDBX storage")?;
        storage
            .set_initial_state_anchor(BlockNumHash::new(0, B256::ZERO))
            .wrap_err("failed to set initial state anchor")?;
        storage.commit_initial_state().wrap_err("failed to commit initial state")?;
        Ok(Self { storage })
    }
}

impl BenchBackend for ApiMdbxBackend {
    fn name(&self) -> &'static str {
        "api-mdbx"
    }

    fn write_block(&self, rows: &[ProofRow]) -> Result<()> {
        let Some(first) = rows.first() else {
            return Ok(());
        };

        let block_number = first.block_number;
        let parent = if block_number == 1 { B256::ZERO } else { block_hash(block_number - 1) };
        let block = NumHash::new(block_number, block_hash(block_number));
        let block_ref = BlockWithParent::new(parent, block);
        let block_state_diff = BlockStateDiff {
            sorted_trie_updates: Default::default(),
            sorted_post_state: rows_to_post_state(rows),
        };

        self.storage.store_trie_updates(block_ref, block_state_diff)?;
        Ok(())
    }

    fn read_existing(&self, row: &ProofRow) -> Result<bool> {
        let mut cursor = self.storage.storage_hashed_cursor(row.address, row.block_number)?;
        Ok(cursor.seek(row.slot)?.is_some_and(|(slot, _)| slot == row.slot))
    }
}

struct ApiRocksdbBackend {
    storage: RocksdbProofsStorage,
}

impl ApiRocksdbBackend {
    fn open(path: &Path) -> Result<Self> {
        let storage =
            RocksdbProofsStorage::new(path).wrap_err("failed to open API RocksDB storage")?;
        storage
            .set_initial_state_anchor(BlockNumHash::new(0, B256::ZERO))
            .wrap_err("failed to set initial state anchor")?;
        storage.commit_initial_state().wrap_err("failed to commit initial state")?;
        Ok(Self { storage })
    }
}

impl BenchBackend for ApiRocksdbBackend {
    fn name(&self) -> &'static str {
        "api-rocksdb"
    }

    fn write_block(&self, rows: &[ProofRow]) -> Result<()> {
        let Some(first) = rows.first() else {
            return Ok(());
        };

        let block_number = first.block_number;
        let parent = if block_number == 1 { B256::ZERO } else { block_hash(block_number - 1) };
        let block = NumHash::new(block_number, block_hash(block_number));
        let block_ref = BlockWithParent::new(parent, block);
        let block_state_diff = BlockStateDiff {
            sorted_trie_updates: Default::default(),
            sorted_post_state: rows_to_post_state(rows),
        };

        self.storage.store_trie_updates(block_ref, block_state_diff)?;
        Ok(())
    }

    fn read_existing(&self, row: &ProofRow) -> Result<bool> {
        let mut cursor = self.storage.storage_hashed_cursor(row.address, row.block_number)?;
        Ok(cursor.seek(row.slot)?.is_some_and(|(slot, _)| slot == row.slot))
    }
}

struct RawRocksdbBackend {
    db: DBWithThreadMode<MultiThreaded>,
    write_options: WriteOptions,
}

impl RawRocksdbBackend {
    fn open(path: &Path, config: &Config) -> Result<Self> {
        let mut db_options = Options::default();
        db_options.create_if_missing(true);
        db_options.create_missing_column_families(true);
        db_options.enable_statistics();
        db_options.set_max_background_jobs(8);

        let mut cf_options = Options::default();
        cf_options.set_compression_type(DBCompressionType::None);
        cf_options.set_level_compaction_dynamic_level_bytes(true);
        cf_options.set_max_write_buffer_number(6);
        cf_options.set_target_file_size_base(256 * 1024 * 1024);
        cf_options.set_write_buffer_size(256 * 1024 * 1024);

        let descriptors = vec![ColumnFamilyDescriptor::new(HASHED_STORAGE_HISTORY_CF, cf_options)];
        let db =
            DBWithThreadMode::<MultiThreaded>::open_cf_descriptors(&db_options, path, descriptors)
                .wrap_err("failed to open RocksDB database")?;

        let mut write_options = WriteOptions::default();
        write_options.set_sync(config.rocksdb_sync);

        Ok(Self { db, write_options })
    }

    fn cf(&self) -> Result<Arc<rocksdb::BoundColumnFamily<'_>>> {
        self.db
            .cf_handle(HASHED_STORAGE_HISTORY_CF)
            .ok_or_else(|| eyre!("missing RocksDB column family {HASHED_STORAGE_HISTORY_CF}"))
    }
}

impl BenchBackend for RawRocksdbBackend {
    fn name(&self) -> &'static str {
        "raw-rocksdb"
    }

    fn write_block(&self, rows: &[ProofRow]) -> Result<()> {
        let cf = self.cf()?;
        let mut batch = WriteBatch::default();

        for row in rows {
            batch.put_cf(&cf, rocksdb_key(row), rocksdb_value(row));
        }

        self.db.write_opt(batch, &self.write_options)?;
        Ok(())
    }

    fn read_existing(&self, row: &ProofRow) -> Result<bool> {
        let cf = self.cf()?;
        Ok(self.db.get_cf(&cf, rocksdb_key(row))?.is_some())
    }

    fn prune_block(&self, rows: &[ProofRow]) -> Result<PruneCounts> {
        let cf = self.cf()?;
        let mut batch = WriteBatch::default();
        let mut counts = PruneCounts::default();

        for row in rows {
            counts.scanned += 1;
            let key = rocksdb_key(row);
            if self.db.get_cf(&cf, key)?.is_some() {
                counts.found += 1;
                batch.delete_cf(&cf, key);
                counts.deleted += 1;
            }
        }

        self.db.write_opt(batch, &self.write_options)?;
        Ok(counts)
    }

    fn finish(&self) -> Result<()> {
        let cf = self.cf()?;
        self.db.flush_cf(&cf)?;
        Ok(())
    }

    fn stats(&self) -> Result<Vec<(String, String)>> {
        let cf = self.cf()?;
        let mut stats = Vec::new();
        for key in [
            "rocksdb.estimate-pending-compaction-bytes",
            "rocksdb.cur-size-all-mem-tables",
            "rocksdb.mem-table-flush-pending",
            "rocksdb.compaction-pending",
            "rocksdb.num-running-compactions",
        ] {
            if let Some(value) = self.db.property_value_cf(&cf, key)? {
                stats.push((key.to_string(), value));
            }
        }
        Ok(stats)
    }
}

fn rows_to_post_state(rows: &[ProofRow]) -> reth_trie_common::HashedPostStateSorted {
    let mut state = HashedPostState::default();
    for row in rows {
        state
            .storages
            .entry(row.address)
            .or_insert_with(|| HashedStorage::new(false))
            .storage
            .insert(row.slot, row.value);
    }
    state.into_sorted()
}

fn generate_block(config: &Config, first_row_index: u64, len: usize) -> Vec<ProofRow> {
    let mut rows: Vec<_> =
        (0..len).map(|offset| row_for_index(config, first_row_index + offset as u64)).collect();
    rows.sort_unstable();
    rows
}

fn row_for_index(config: &Config, row_index: u64) -> ProofRow {
    let account_index =
        mix(config.seed ^ row_index.wrapping_mul(0x9e37_79b9_7f4a_7c15)) % config.accounts;
    let address = b256_from_seed(config.seed ^ account_index ^ 0x9ddf_ea08_eb38_2d69);
    let slot = b256_from_seed(config.seed ^ row_index ^ 0x94d0_49bb_1331_11eb);
    let value =
        U256::from_be_bytes(b256_from_seed(config.seed ^ row_index ^ 0x3c79_ac49_2ba7_b653).0);
    let block_number = block_number_for_row(row_index, config);

    ProofRow { address, slot, value, block_number }
}

const fn block_number_for_row(row_index: u64, config: &Config) -> u64 {
    row_index / config.writes_per_block as u64 + 1
}

const fn blocks_for_rows(rows: u64, config: &Config) -> u64 {
    rows.div_ceil(config.writes_per_block as u64)
}

const fn oldest_readable_row(config: &Config, committed_rows: u64) -> u64 {
    if !config.pruner {
        return 0;
    }

    let retained_rows = config.prune_window_blocks.saturating_mul(config.writes_per_block as u64);
    committed_rows.saturating_sub(retained_rows)
}

fn block_hash(block_number: u64) -> B256 {
    if block_number == 0 {
        return B256::ZERO;
    }
    b256_from_seed(0xb10c_0000_0000_0000 ^ block_number)
}

fn rocksdb_key(row: &ProofRow) -> [u8; 72] {
    let hashed_storage_key = HashedStorageKey::new(row.address, row.slot).encode();
    let mut key = [0u8; 72];
    key[..64].copy_from_slice(&hashed_storage_key);
    key[64..].copy_from_slice(&row.block_number.to_be_bytes());
    key
}

fn rocksdb_value(row: &ProofRow) -> Vec<u8> {
    VersionedValue::new(row.block_number, MaybeDeleted(Some(StorageValue::new(row.value))))
        .compress()
}

#[derive(Debug, Clone, Copy)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    const fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        mix(self.state)
    }

    const fn next_bounded(&mut self, upper_bound: u64) -> u64 {
        if upper_bound == 0 {
            return 0;
        }
        self.next_u64() % upper_bound
    }
}

const fn mix(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn b256_from_seed(seed: u64) -> B256 {
    let mut bytes = [0u8; 32];
    let mut rng = SplitMix64::new(seed);
    for chunk in bytes.chunks_exact_mut(8) {
        chunk.copy_from_slice(&rng.next_u64().to_be_bytes());
    }
    B256::from(bytes)
}

fn print_summary(
    label: &str,
    rows: u64,
    latencies: &[Duration],
    reader_stats: Option<ReaderStats>,
    background_writer_stats: Option<WriterStats>,
    pruner_stats: Option<PrunerStats>,
) {
    let summary = LatencySummary::new(latencies);
    let rows_per_second =
        if summary.total.is_zero() { 0.0 } else { rows as f64 / summary.total.as_secs_f64() };

    println!("{label}");
    println!("    blocks                 {}", latencies.len());
    println!("    rows                   {rows}");
    println!("    total_seconds          {:.3}", summary.total.as_secs_f64());
    println!("    rows_per_second        {rows_per_second:.0}");
    println!("    min_seconds            {:.6}", seconds(summary.min));
    println!("    p50_seconds            {:.6}", seconds(summary.p50));
    println!("    p90_seconds            {:.6}", seconds(summary.p90));
    println!("    p95_seconds            {:.6}", seconds(summary.p95));
    println!("    p99_seconds            {:.6}", seconds(summary.p99));
    println!("    p999_seconds           {:.6}", seconds(summary.p999));
    println!("    max_seconds            {:.6}", seconds(summary.max));

    if let Some(stats) = reader_stats {
        let read_rate = if stats.duration.is_zero() {
            0.0
        } else {
            stats.operations as f64 / stats.duration.as_secs_f64()
        };
        println!("    reader_seconds         {:.3}", stats.duration.as_secs_f64());
        println!("    reader_ops             {}", stats.operations);
        println!("    reader_ops_per_second  {read_rate:.0}");
        println!("    reader_hits            {}", stats.hits);
        println!("    reader_misses          {}", stats.misses);
    }

    if let Some(stats) = background_writer_stats
        && stats.blocks > 0
    {
        let write_rate = if stats.duration.is_zero() {
            0.0
        } else {
            stats.rows as f64 / stats.duration.as_secs_f64()
        };
        let summary = LatencySummary::new(&stats.latencies);
        println!("    background_writer_seconds {:.3}", stats.duration.as_secs_f64());
        println!("    background_writer_blocks  {}", stats.blocks);
        println!("    background_writer_rows    {}", stats.rows);
        println!("    background_writer_rows_per_second {write_rate:.0}");
        println!("    background_writer_p50_seconds {:.6}", seconds(summary.p50));
        println!("    background_writer_p95_seconds {:.6}", seconds(summary.p95));
        println!("    background_writer_p99_seconds {:.6}", seconds(summary.p99));
        println!("    background_writer_max_seconds {:.6}", seconds(summary.max));
    }

    if let Some(stats) = pruner_stats
        && stats.blocks > 0
    {
        let scan_rate = if stats.duration.is_zero() {
            0.0
        } else {
            stats.rows_scanned as f64 / stats.duration.as_secs_f64()
        };
        let summary = LatencySummary::new(&stats.latencies);
        println!("    pruner_seconds        {:.3}", stats.duration.as_secs_f64());
        println!("    pruner_blocks         {}", stats.blocks);
        println!("    pruner_rows_scanned   {}", stats.rows_scanned);
        println!("    pruner_rows_found     {}", stats.rows_found);
        println!("    pruner_rows_deleted   {}", stats.rows_deleted);
        println!("    pruner_rows_per_second {scan_rate:.0}");
        println!("    pruner_p50_seconds    {:.6}", seconds(summary.p50));
        println!("    pruner_p95_seconds    {:.6}", seconds(summary.p95));
        println!("    pruner_p99_seconds    {:.6}", seconds(summary.p99));
        println!("    pruner_max_seconds    {:.6}", seconds(summary.max));
    }
}

const fn seconds(duration: Duration) -> f64 {
    duration.as_secs_f64()
}

#[derive(Debug, Clone, Copy)]
struct LatencySummary {
    max: Duration,
    min: Duration,
    p50: Duration,
    p90: Duration,
    p95: Duration,
    p99: Duration,
    p999: Duration,
    total: Duration,
}

impl LatencySummary {
    fn new(latencies: &[Duration]) -> Self {
        if latencies.is_empty() {
            return Self {
                max: Duration::ZERO,
                min: Duration::ZERO,
                p50: Duration::ZERO,
                p90: Duration::ZERO,
                p95: Duration::ZERO,
                p99: Duration::ZERO,
                p999: Duration::ZERO,
                total: Duration::ZERO,
            };
        }

        let mut sorted = latencies.to_vec();
        sorted.sort_unstable();
        let total = latencies.iter().copied().sum();
        Self {
            max: *sorted.last().expect("non-empty latencies"),
            min: sorted[0],
            p50: percentile(&sorted, 0.50),
            p90: percentile(&sorted, 0.90),
            p95: percentile(&sorted, 0.95),
            p99: percentile(&sorted, 0.99),
            p999: percentile(&sorted, 0.999),
            total,
        }
    }
}

fn percentile(sorted: &[Duration], percentile: f64) -> Duration {
    let len = sorted.len();
    let index = ((len as f64 * percentile).ceil() as usize).saturating_sub(1).min(len - 1);
    sorted[index]
}

#[derive(Debug, Clone, Copy, Default)]
struct DirectorySizes {
    allocated_bytes: u64,
    apparent_bytes: u64,
}

impl std::ops::AddAssign for DirectorySizes {
    fn add_assign(&mut self, rhs: Self) {
        self.allocated_bytes += rhs.allocated_bytes;
        self.apparent_bytes += rhs.apparent_bytes;
    }
}

fn directory_sizes(path: &Path) -> Result<DirectorySizes> {
    let mut total = DirectorySizes::default();
    for entry in
        fs::read_dir(path).wrap_err_with(|| format!("failed to read {}", path.display()))?
    {
        total += entry_sizes(entry?)?;
    }
    Ok(total)
}

fn entry_sizes(entry: DirEntry) -> Result<DirectorySizes> {
    let metadata = entry.metadata()?;
    if metadata.is_file() {
        return Ok(DirectorySizes {
            allocated_bytes: allocated_bytes(&metadata),
            apparent_bytes: metadata.len(),
        });
    }

    if metadata.is_dir() {
        return directory_sizes(&entry.path());
    }

    Ok(DirectorySizes::default())
}

#[cfg(unix)]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;

    metadata.blocks() * 512
}

#[cfg(not(unix))]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    metadata.len()
}
