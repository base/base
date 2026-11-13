//! Disposable-run admission and fail-stop long-range sync for deferred commitments.
//!
//! Markers live next to the chain datadir, outside all binary-reset store directories. They are
//! never automatically deleted, including after clean shutdown or a failed preflight.

use std::{collections::BTreeMap, fs, io::{Read, Write}, path::{Path, PathBuf}, sync::Arc, task::{Context, Poll}};
use alloy_primitives::{B256, Keccak256};
use base_common_consensus::BasePrimitives;
use base_execution_chainspec::BaseChainSpec;
use base_execution_payload_builder::{PostStateRootCohort, PostStateRootLimits, PostStateRootService};
use base_node_core::args::RollupArgs;
use eyre::{Result, ensure};
use reth_db::{Database, Tables, transaction::DbTx, tables_to_generic};
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_engine_tree::backfill::{BackfillAction, BackfillEvent, BackfillSync, PipelineSync};
use reth_node_builder::{NodeConfig, sync::{BackfillContext, BackfillSyncBuilder, PipelineBackfill}};
use reth_provider::{BlockNumReader, ChainStateBlockReader, HeaderProvider, LatestStateProviderRef, MetadataProvider, ProviderFactory, PruneCheckpointReader, RocksDBProviderFactory, StageCheckpointReader, StateRootProvider, StaticFileProviderFactory, StaticFileSegment, StorageSettingsCache, providers::{ProviderNodeTypes, StaticFileProviderBuilder}};
use reth_stages_types::StageId;
use serde::{Deserialize, Serialize};

/// Immutable snapshot provenance recorded independently from resettable store directories.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeferredStateRootSnapshot {
    /// Human-readable source/revision establishing the snapshot's known consistent provenance.
    pub provenance: String,
    /// Chain anchor identity.
    pub genesis_hash: B256,
    /// Strictly pre-fork tip identity.
    pub tip_hash: B256,
    /// Strictly pre-fork absolute height.
    pub tip_number: u64,
    /// Exact relative file names and streaming Keccak digests for all three store trees.
    pub files: BTreeMap<String, B256>,
}

/// Single-use, unforgeable handoff from read-only admission before writable MDBX initialization.
#[derive(Debug)]
pub struct DeferredStateRootPrepared {
    admission: DeferredStateRootAdmission,
    binding: B256,
}

impl DeferredStateRootPrepared {
    /// Binds the one-shot admission to the full chain, arguments, canonical path and manifest bytes.
    fn binding(config: &NodeConfig<BaseChainSpec>, args: &RollupArgs) -> Result<B256> {
        let path = fs::canonicalize(config.datadir().data_dir())?;
        let manifest = args.deferred_state_root_snapshot.as_ref().map(fs::read).transpose()?;
        let marker = fs::read(DeferredStateRootAdmission::marker_path(config.datadir().data_dir())?)?;
        Ok(alloy_primitives::keccak256(format!("{:?}{:?}{:?}{:?}{:?}", config.chain, args, path, manifest, marker)))
    }

    /// Refuses generic upstream CLI initialization, which cannot carry the sealed admission.
    pub fn ensure_generic_launch_allowed(chain: &BaseChainSpec, args: &RollupArgs) -> Result<()> {
        ensure!(chain.validate_deferred_state_root_schedule()?.is_none() && args.root_experiment_cohort.is_none_or(|cohort| cohort == PostStateRootCohort::Ordinary) && !args.deferred_state_root_disposable && args.deferred_state_root_snapshot.is_none(), "root experiments require the admitted launch API before writable database initialization");
        Ok(())
    }

    /// Authenticates and checks stores before init_db can write; ordinary startup is unchanged.
    pub fn prepare_database(config: &NodeConfig<BaseChainSpec>, args: &RollupArgs, runtime: reth_tasks::Runtime) -> Result<(reth_db::DatabaseEnv, Option<Self>)> {
        let admission = DeferredStateRootAdmission::preflight(config, args, 0)?;
        if let Some(admission) = admission {
            let datadir = config.datadir();
            if datadir.db().join("mdbx.dat").exists() {
                let db = reth_db::open_db_read_only(datadir.db(), config.db.database_args())?;
                admission.verify_before_launch(config, db, runtime)?;
            } else { ensure!(admission.snapshot.is_none(), "authenticated snapshot is missing MDBX"); }
            fs::create_dir_all(datadir.data_dir())?;
            let binding = Self::binding(config, args)?;
            let db = reth_db::init_db(datadir.db(), config.db.database_args())?;
            Ok((db, Some(Self { admission, binding })))
        } else { Ok((reth_db::init_db(config.datadir().db(), config.db.database_args())?, None)) }
    }

    /// Rechecks all configuration gates and the snapshot identity without rehashing authorized writes.
    pub fn consume(self, config: &NodeConfig<BaseChainSpec>, args: &RollupArgs, extensions: usize) -> Result<DeferredStateRootAdmission> {
        ensure!(self.binding == Self::binding(config, args)?, "prepared root admission does not match chain, datadir, schedule, cohort or snapshot");
        DeferredStateRootAdmission::preflight_inner(config, args, extensions, Some(self.admission))?.ok_or_else(|| eyre::eyre!("prepared experiment lost its admission"))
    }
}

/// Admitted disposable configuration and immutable snapshot provenance.
#[derive(Debug, Clone)]
pub struct DeferredStateRootAdmission {
    /// Shared root service, initialized only when an active block executes.
    pub service: Arc<PostStateRootService>,
    /// Persistent marker outside the chain datadir. Never removed automatically.
    pub marker: PathBuf,
    /// Verified immutable pre-fork snapshot, or fresh genesis.
    pub snapshot: Option<DeferredStateRootSnapshot>,
}

impl DeferredStateRootAdmission {
    /// Computes the marker path without creating or resetting any state.
    pub fn marker_path(datadir: &Path) -> Result<PathBuf> {
        let name = datadir.file_name().ok_or_else(|| eyre::eyre!("experimental datadir must have a final path component"))?;
        Ok(datadir.with_file_name(format!(".{}.deferred-state-root-run", name.to_string_lossy())))
    }

    /// Hashes every regular store file with bounded memory, rejecting links and unknown layouts.
    pub fn store_digests(datadir: &Path) -> Result<BTreeMap<String, B256>> {
        let mut files = BTreeMap::new(); let mut pending = Vec::new();
        for store in ["db", "static_files", "rocksdb"] {
            let path = datadir.join(store); if path.exists() { pending.push(path); }
        }
        while let Some(path) = pending.pop() {
            let metadata = fs::symlink_metadata(&path)?;
            ensure!(!metadata.file_type().is_symlink(), "snapshot stores must not contain symlinks");
            if metadata.is_dir() {
                for entry in fs::read_dir(&path)? { pending.push(entry?.path()); }
            } else {
                ensure!(metadata.is_file(), "snapshot stores must contain only directories and regular files");
                // OS locks are not immutable chain data. Every other byte is authenticated.
                if matches!(path.file_name().and_then(|s| s.to_str()), Some("mdbx.lck" | "LOCK")) { continue; }
                let mut file = fs::File::open(&path)?; let mut hash = Keccak256::new(); let mut buffer = [0u8; 65536];
                loop { let count = file.read(&mut buffer)?; if count == 0 { break; } hash.update(&buffer[..count]); }
                files.insert(path.strip_prefix(datadir)?.to_string_lossy().into_owned(), hash.finalize());
            }
        }
        Ok(files)
    }

    /// Runs before reth launch/recovery. A configured fork rejects even pre-fork incompatible use.
    pub fn preflight(config: &NodeConfig<BaseChainSpec>, args: &RollupArgs, extension_count: usize) -> Result<Option<Self>> {
        Self::preflight_inner(config, args, extension_count, None)
    }

    /// Reuses authenticated provenance only for the private, configuration-bound early handoff.
    fn preflight_inner(config: &NodeConfig<BaseChainSpec>, args: &RollupArgs, extension_count: usize, prepared: Option<Self>) -> Result<Option<Self>> {
        let activation = config.chain.validate_deferred_state_root_schedule()?;
        let cohort = args.root_experiment_cohort.unwrap_or(if activation.is_some() { PostStateRootCohort::Deferred } else { PostStateRootCohort::Ordinary });
        if activation.is_none() && cohort == PostStateRootCohort::Ordinary { return Ok(None); }
        ensure!(activation.is_some() == (cohort == PostStateRootCohort::Deferred), "DeferredStateRoot schedule must select deferred cohort; serial controls require an unscheduled custom chain");
        ensure!(!matches!(config.chain.chain().id(), 8453 | 84532), "root experiments forbid mainnet and Sepolia");
        ensure!(args.root_experiment_queue > 0 && args.root_experiment_ready >= 2 && args.root_experiment_timeout_ms > 0, "root experiment limits require positive queue/deadline and at least two ready entries");
        ensure!(args.deferred_state_root_disposable, "DeferredStateRoot requires --rollup.deferred-state-root-disposable");
        ensure!(!config.debug.skip_state_root, "DeferredStateRoot is incompatible with debug.skip-state-root");
        ensure!(!config.network.snap_v2, "DeferredStateRoot forbids snap serving and snap sync");
        ensure!(config.prune_config().is_none(), "DeferredStateRoot requires unpruned disposable state");
        ensure!(config.debug.invalid_block_hook.as_ref().is_none_or(|hooks| hooks.is_empty()), "DeferredStateRoot is incompatible with configured invalid-block diagnostic hooks, including witness");
        ensure!(!args.proofs_history && args.proofs_history_storage_path.is_none(), "DeferredStateRoot forbids proof history services");
        ensure!(args.upgrade_signal.contract_address.is_none() && args.upgrade_signal_l1_rpc.upgrade_signal_l1_rpc.is_none(), "DeferredStateRoot forbids L1 upgrade-signal RPC");
        ensure!(args.sequencer.is_none(), "DeferredStateRoot forbids production sequencer forwarding");
        ensure!(extension_count == 0, "DeferredStateRoot forbids unverified node extensions and production proof/output/L1 services");
        let datadir = config.datadir();
        ensure!(datadir.static_files() == datadir.data_dir().join("static_files") && datadir.rocksdb() == datadir.data_dir().join("rocksdb"), "DeferredStateRoot requires all stores under the disposable datadir");
        let marker = Self::marker_path(datadir.data_dir())?;
        if let Some(prepared) = prepared {
            ensure!(prepared.marker == marker && marker.exists(), "prepared root admission marker is missing or mismatched");
            return Ok(Some(prepared));
        }
        ensure!(!marker.exists(), "DeferredStateRoot datadir has already been used; restore a new numbered snapshot copy, never remove its marker");
        let snapshot = args.deferred_state_root_snapshot.as_ref().map(|path| -> Result<DeferredStateRootSnapshot> {
            let snapshot: DeferredStateRootSnapshot = serde_json::from_slice(&fs::read(path)?)?;
            ensure!(!snapshot.provenance.trim().is_empty(), "snapshot provenance must not be empty");
            ensure!(snapshot.genesis_hash == config.chain.genesis_hash(), "snapshot genesis hash must match the configured chain");
            ensure!(snapshot.files == Self::store_digests(datadir.data_dir())?, "snapshot store files must exactly match the immutable manifest");
            Ok(snapshot)
        }).transpose()?;
        ensure!(snapshot.is_some() || (!datadir.static_files().exists() && !datadir.rocksdb().exists()), "unmarked existing stores require a verified immutable pre-fork snapshot manifest");
        if let Some(parent) = marker.parent() { fs::create_dir_all(parent)?; }
        let mut record = fs::OpenOptions::new().write(true).create_new(true).open(&marker)?;
        writeln!(record, "chain={} genesis={} activation={:?} cohort={:?} queue={} ready={} timeout_ms={} snapshot={:?}", config.chain.chain().id(), config.chain.genesis_hash(), activation, cohort, args.root_experiment_queue, args.root_experiment_ready, args.root_experiment_timeout_ms, snapshot)?;
        record.sync_all()?;
        if let Some(parent) = marker.parent() { fs::File::open(parent)?.sync_all()?; }
        Ok(Some(Self { service: Arc::new(PostStateRootService { activation, cohort, limits: PostStateRootLimits { queued: args.root_experiment_queue, ready: args.root_experiment_ready, timeout: std::time::Duration::from_millis(args.root_experiment_timeout_ms) }, ..Default::default() }), marker, snapshot }))
    }

    /// Reads MDBX only through RO provider transactions on the caller-owned environment,
    /// with explicitly read-only static/Rocks handles before reth opens those stores writable.
    pub fn verify_before_launch(&self, config: &NodeConfig<BaseChainSpec>, db: reth_db::DatabaseEnv, runtime: reth_tasks::Runtime) -> Result<()> {
        if self.snapshot.is_none() {
            let tx = db.tx()?;
            for table in reth_db::Tables::ALL {
                // init_db records the executable version even in an otherwise empty database.
                if matches!(table, Tables::VersionHistory) { continue; }
                ensure!(tables_to_generic!(table, |T| tx.entries::<T>())? == 0, "unmarked MDBX state requires immutable snapshot provenance");
            }
            return Ok(());
        }
        let datadir = config.datadir();
        let static_files = StaticFileProviderBuilder::read_only(datadir.static_files())
            .with_genesis_block_number(config.chain.genesis().number.unwrap_or_default()).build::<BasePrimitives>()?;
        let rocksdb = reth_provider::providers::RocksDBProvider::builder(datadir.rocksdb()).with_default_tables().with_read_only(true).build()?;
        let factory = ProviderFactory::<reth_node_builder::NodeTypesWithDBAdapter<crate::BaseNode, _>>::new(db, config.chain.clone(), static_files, rocksdb, runtime)?;
        self.verify_before_recovery(&factory)
    }

    /// Rejects active/unmarked or inconsistent checkpoint state before stock multi-store recovery.
    /// Does not call ProviderFactory::check_consistency, which can heal/write storage.
    pub fn verify_before_recovery<N>(&self, factory: &ProviderFactory<N>) -> Result<()>
    where N: ProviderNodeTypes<Primitives = BasePrimitives, ChainSpec = BaseChainSpec>,
    {
        let provider = factory.provider()?;
        ensure!(provider.pipeline_unwind()?.is_none() && provider.snap_attempt()?.is_none(), "experimental store has pending unwind or snap recovery");
        ensure!(provider.get_metadata("partial_state_trie_unwind")?.is_none(), "experimental store has partial-trie unwind recovery");
        let tip = provider.last_block_number()?;
        ensure!(provider.get_prune_checkpoints()?.is_empty(), "experimental snapshots must be unpruned");
        ensure!(provider.last_safe_block_number()?.is_none_or(|n| n <= tip) && provider.last_finalized_block_number()?.is_none_or(|n| n <= tip), "experimental snapshot safe/finalized metadata exceeds its tip");
        if tip == 0 && provider.cached_storage_settings().storage_v2 {
            ensure!(provider.rocksdb_provider().first::<reth_db::tables::TransactionHashNumbers>()?.is_none(), "experimental genesis has stale RocksDB transaction lookup data");
        }
        if tip > 0 {
            ensure!(provider.get_stage_checkpoint(StageId::Finish)?.is_some_and(|checkpoint| checkpoint.block_number == tip), "experimental snapshot must have an exact durable Finish checkpoint");
        }
        let header = provider.header_by_number(tip)?;
        let activation = self.service.activation.unwrap_or(u64::MAX);
        if let Some(header) = &header { ensure!(header.timestamp < activation, "unmarked database already contains active history"); }
        for (stage, checkpoint) in provider.get_all_checkpoints()? {
            ensure!(checkpoint.block_number == tip, "experimental snapshot has inconsistent stage checkpoint {stage}");
        }
        if let Some(finish) = provider.get_stage_checkpoint(StageId::Finish)? {
            ensure!(finish.finish_stage_checkpoint().and_then(|c| c.partial_state_trie()).is_none_or(|n| n == tip), "experimental snapshot has partial state trie lag");
        }
        let static_files = factory.static_file_provider();
        // Reuse pinned file checks only through an explicitly read-only view. Calling these
        // methods on the factory's writable provider would silently heal interrupted writes.
        let readonly = StaticFileProviderBuilder::read_only(static_files.directory()).with_genesis_block_number(factory.chain_spec().genesis().number.unwrap_or_default()).build::<BasePrimitives>()?;
        readonly.check_file_consistency(&provider)?;
        for segment in [StaticFileSegment::Headers, StaticFileSegment::Transactions, StaticFileSegment::Receipts, StaticFileSegment::TransactionSenders, StaticFileSegment::AccountChangeSets, StaticFileSegment::StorageChangeSets] {
            let frontier = static_files.get_highest_static_file_block(segment);
            ensure!(frontier.is_none_or(|height| height <= tip), "experimental snapshot has static files ahead of its checkpoint");
            if provider.cached_storage_settings().storage_v2 || matches!(segment, StaticFileSegment::Headers | StaticFileSegment::Transactions) {
                ensure!(frontier.unwrap_or_default() == tip, "experimental snapshot static segment {segment:?} lags its checkpoint");
            }
            if let Some(height) = frontier {
                let jar = readonly.get_segment_provider_for_block(segment, height, None)?;
                if segment.is_change_based() {
                    let offsets = jar.read_changeset_offsets()?.ok_or_else(|| eyre::eyre!("experimental snapshot lacks committed changeset offsets"))?;
                    let mut next = 0;
                    for offset in &offsets {
                        ensure!(offset.offset() == next, "experimental snapshot changeset offsets are discontinuous");
                        next = offset.changeset_range().end;
                    }
                    ensure!(next == jar.rows() as u64, "experimental snapshot changeset rows disagree with committed offsets");
                    // Each sidecar record encodes the start offset and change count as two u64s.
                    let record_bytes = 2 * std::mem::size_of::<u64>() as u64;
                    let path = jar.data_path().with_extension("csoff");
                    ensure!(fs::metadata(path).map(|m| m.len()).unwrap_or_default() == offsets.len() as u64 * record_bytes, "experimental snapshot changeset sidecar contains uncommitted or truncated records");
                }
            }
        }
        match &self.snapshot {
            Some(snapshot) => {
                ensure!(tip == snapshot.tip_number, "snapshot tip height must exactly match immutable provenance");
                let header = header.ok_or_else(|| eyre::eyre!("snapshot tip header is missing"))?;
                ensure!(header.hash_slow() == snapshot.tip_hash, "snapshot tip hash must exactly match immutable provenance");
                let root = LatestStateProviderRef::new(&provider).state_root(Default::default())?;
                ensure!(root == header.state_root, "pre-fork snapshot actual MPT root must equal its legacy header");
            }
            None => {
                ensure!(tip == factory.chain_spec().genesis().number.unwrap_or_default(), "unmarked non-genesis database requires immutable pre-fork snapshot provenance");
                if let Some(header) = header {
                    ensure!(header.hash_slow() == factory.chain_spec().genesis_hash(), "fresh genesis database must match the configured chain anchor");
                    ensure!(LatestStateProviderRef::new(&provider).state_root(Default::default())? == header.state_root, "fresh genesis actual state must match its legacy commitment");
                }
            }
        }
        Ok(())
    }
}

/// Stock pipeline outside experiments, fatal refusal before any stage runs inside experiments.
#[derive(Debug)]
pub enum DeferredStateRootBackfill<N: ProviderNodeTypes> {
    /// Untouched ordinary staged sync implementation.
    Stock(PipelineSync<N>),
    /// Experimental replay-only mode; a long-range sync request is fatal.
    Refuse { /// Whether a forbidden start request has arrived.
        requested: bool },
}

impl<N: ProviderNodeTypes> BackfillSync for DeferredStateRootBackfill<N> {
    fn on_action(&mut self, action: BackfillAction) {
        match self {
            Self::Stock(stock) => stock.on_action(action),
            Self::Refuse { requested } => { if matches!(action, BackfillAction::Start(_)) { *requested = true; } }
        }
    }
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<BackfillEvent> {
        match self {
            Self::Stock(stock) => stock.poll(cx),
            Self::Refuse { requested } if *requested => { *requested = false; Poll::Ready(BackfillEvent::TaskDropped("DeferredStateRoot forbids staged/snap/backfill/deep-reorg fallback".into())) }
            Self::Refuse { .. } => Poll::Pending,
        }
    }
}

/// Installs replay-only refusal and the pre-recovery snapshot guard without modifying reth.
#[derive(Debug, Clone, Default)]
pub struct DeferredStateRootBackfillBuilder {
    /// None preserves ordinary startup and sync behavior exactly.
    pub admission: Option<DeferredStateRootAdmission>,
}

impl<N, C> BackfillSyncBuilder<N, C> for DeferredStateRootBackfillBuilder
where N: ProviderNodeTypes<Primitives = BasePrimitives, ChainSpec = BaseChainSpec>, C: Send,
{
    type Backfill = DeferredStateRootBackfill<N>;
    fn recover(&mut self, factory: &ProviderFactory<N>) -> Result<()> {
        if let Some(admission) = &self.admission { admission.verify_before_recovery(factory)?; }
        Ok(())
    }
    fn build(self, ctx: BackfillContext<N, C>) -> Result<Self::Backfill> {
        if self.admission.is_some() { Ok(DeferredStateRootBackfill::Refuse { requested: false }) }
        else { Ok(DeferredStateRootBackfill::Stock(PipelineBackfill.build(ctx)?)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base_execution_chainspec::BaseChainSpecBuilder;
    use base_common_genesis::BaseUpgrade;
    use reth_chainspec::{Chain, ForkCondition};
    use reth_node_core::args::DatadirArgs;
    use reth_provider::{test_utils::create_test_provider_factory_with_node_types, ChainStateBlockWriter, StageCheckpointWriter, StaticFileWriter};
    use reth_stages_types::StageCheckpoint;

    /// Builds a supported custom schedule without activating the experimental rule at genesis.
    fn chain() -> BaseChainSpec {
        BaseChainSpecBuilder::default().chain(Chain::from(900u64)).genesis(Default::default())
            .bedrock_activated().with_fork(BaseUpgrade::Regolith, ForkCondition::Timestamp(0))
            .with_fork(BaseUpgrade::DeferredStateRoot, ForkCondition::Timestamp(10)).build()
    }
    /// Gives each admission attempt an independent numbered disposable datadir.
    fn config(path: &Path) -> NodeConfig<BaseChainSpec> {
        let mut config = NodeConfig::new(Arc::new(chain())).with_datadir_args(DatadirArgs { datadir: path.to_path_buf().into(), ..Default::default() });
        config.debug.invalid_block_hook = None;
        config
    }
    /// Makes the irreversible disposable-run acknowledgement explicit in every admission test.
    fn args() -> RollupArgs { RollupArgs { deferred_state_root_disposable: true, ..Default::default() } }

    /// Legacy generic CLI cannot initialize writable stores for any experimental input.
    #[test]
    fn generic_cli_refuses_all_experiments_before_database_initialization() {
        let ordinary = BaseChainSpecBuilder::default().chain(Chain::from(900u64)).genesis(Default::default()).bedrock_activated().build();
        DeferredStateRootPrepared::ensure_generic_launch_allowed(&ordinary, &RollupArgs::default()).unwrap();
        for cohort in [PostStateRootCohort::SerialTrusted, PostStateRootCohort::SerialReexecution, PostStateRootCohort::Deferred] {
            let args = RollupArgs { root_experiment_cohort: Some(cohort), ..Default::default() };
            assert_eq!(DeferredStateRootPrepared::ensure_generic_launch_allowed(&ordinary, &args).unwrap_err().to_string(), "root experiments require the admitted launch API before writable database initialization", "generic CLI must reject each explicit cohort before upstream init_db");
        }
        for args in [RollupArgs { deferred_state_root_disposable: true, ..Default::default() }, RollupArgs { deferred_state_root_snapshot: Some("snapshot.json".into()), ..Default::default() }] {
            assert!(DeferredStateRootPrepared::ensure_generic_launch_allowed(&ordinary, &args).is_err(), "generic CLI must reject snapshot/disposable inputs even without a schedule");
        }
        assert!(DeferredStateRootPrepared::ensure_generic_launch_allowed(&chain(), &RollupArgs::default()).is_err(), "configured D must reject without relying on rollup CLI arguments");
    }

    /// The sealed handoff is one-shot and cannot authorize another datadir or cohort.
    #[test]
    fn prepared_admission_rejects_rebinding() {
        for variant in 0..4 {
            let directory = tempfile::tempdir().unwrap(); let original = config(&directory.path().join("run1"));
            let runtime = reth_tasks::Runtime::test();
            let (db, prepared) = DeferredStateRootPrepared::prepare_database(&original, &args(), runtime.clone()).unwrap(); drop(db);
            let mut config = original.clone(); let mut changed = args();
            match variant {
                0 => changed.root_experiment_ready += 1,
                1 => { config.datadir.datadir = directory.path().join("run2").into(); fs::create_dir_all(config.datadir().data_dir()).unwrap(); fs::write(DeferredStateRootAdmission::marker_path(config.datadir().data_dir()).unwrap(), "other run").unwrap(); },
                2 => changed.root_experiment_cohort = Some(PostStateRootCohort::SerialTrusted),
                _ => { fs::write(DeferredStateRootAdmission::marker_path(config.datadir().data_dir()).unwrap(), "replaced marker").unwrap(); },
            }
            assert_eq!(prepared.unwrap().consume(&config, &changed, 0).unwrap_err().to_string(), "prepared root admission does not match chain, datadir, schedule, cohort or snapshot", "changed limits, datadir, cohort or marker must invalidate the private preparation binding");
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
        }
    }

    /// Both clean and crashed active runs are permanently non-reopenable through their marker.
    #[test]
    fn existing_marker_rejected_before_launch() {
        let directory = tempfile::tempdir().unwrap(); let config = config(&directory.path().join("run1"));
        let admitted = DeferredStateRootAdmission::preflight(&config, &args(), 0).unwrap().unwrap();
        assert!(admitted.marker.exists(), "first admission must durably create the external run marker");
        assert!(!admitted.marker.starts_with(config.datadir().data_dir()), "run marker must remain outside every binary-reset datadir");
        assert!(admitted.service.manager.get().is_none(), "pre-launch admission must construct exactly zero root managers");
        let error = DeferredStateRootAdmission::preflight(&config, &args(), 0).unwrap_err();
        assert!(error.to_string().contains("already been used"), "clean/crashed marker must reject before reth recovery: {error}");
    }

    /// Configuring the fork rejects witness/debug bypass even when activation lies in the future.
    #[test]
    fn pre_fork_incompatible_hooks_rejected() {
        let directory = tempfile::tempdir().unwrap(); let mut config = config(&directory.path().join("run1"));
        config.debug.skip_state_root = true;
        assert_eq!(DeferredStateRootAdmission::preflight(&config, &args(), 0).unwrap_err().to_string(), "DeferredStateRoot is incompatible with debug.skip-state-root", "root bypass rejection is exact");
        config.debug.skip_state_root = false; config.debug.invalid_block_hook = Default::default();
        // Option::default is None; explicitly install the stock witness selection.
        config.debug.invalid_block_hook = Some(Default::default());
        assert_eq!(DeferredStateRootAdmission::preflight(&config, &args(), 0).unwrap_err().to_string(), "DeferredStateRoot is incompatible with configured invalid-block diagnostic hooks, including witness", "witness rejection is exact even before activation");
        assert!(!DeferredStateRootAdmission::marker_path(config.datadir().data_dir()).unwrap().exists(), "configuration rejection must precede marker creation");
    }

    /// Snap serving authenticates proofs against same-height headers and is unsupported too.
    #[test]
    fn snap_serving_rejected_before_launch() {
        let directory = tempfile::tempdir().unwrap(); let mut config = config(&directory.path().join("run1"));
        config.network.snap_v2 = true;
        assert_eq!(DeferredStateRootAdmission::preflight(&config, &args(), 0).unwrap_err().to_string(), "DeferredStateRoot forbids snap serving and snap sync", "configured snap must reject even before the fork");
        assert!(!DeferredStateRootAdmission::marker_path(config.datadir().data_dir()).unwrap().exists(), "snap rejection must precede any store recovery or marker write");
    }

    /// Pruning cannot silently remove retained state required by disposable replay.
    #[test]
    fn pruning_rejected_before_launch() {
        let directory = tempfile::tempdir().unwrap(); let mut config = config(&directory.path().join("run1")); config.pruning.full = true;
        assert_eq!(DeferredStateRootAdmission::preflight(&config, &args(), 0).unwrap_err().to_string(), "DeferredStateRoot requires unpruned disposable state", "explicit pruning must reject before recovery or marker creation");
    }

    /// Unmarked existing multi-store data cannot be admitted without immutable provenance.
    #[test]
    fn unmarked_stores_require_snapshot_provenance() {
        let directory = tempfile::tempdir().unwrap(); let config = config(&directory.path().join("run1"));
        fs::create_dir_all(config.datadir().rocksdb()).unwrap();
        let error = DeferredStateRootAdmission::preflight(&config, &args(), 0).unwrap_err();
        assert_eq!(error.to_string(), "unmarked existing stores require a verified immutable pre-fork snapshot manifest", "unmarked uncertainty must fail closed before recovery");
    }

    /// Hash authentication covers all non-lock store bytes and catches an inconsistent copy.
    #[test]
    fn snapshot_digests_detect_store_changes() {
        let directory = tempfile::tempdir().unwrap(); fs::create_dir(directory.path().join("db")).unwrap();
        fs::write(directory.path().join("db/mdbx.dat"), b"immutable").unwrap();
        let before = DeferredStateRootAdmission::store_digests(directory.path()).unwrap();
        assert_eq!(before.len(), 1, "exactly one immutable store file must be authenticated");
        fs::write(directory.path().join("db/mdbx.dat"), b"changed").unwrap();
        let after = DeferredStateRootAdmission::store_digests(directory.path()).unwrap();
        assert_ne!(before, after, "any store-byte change must invalidate the immutable manifest");
    }

    /// Interrupted static writes must fail read-only admission without stock file healing.
    #[test]
    fn interrupted_static_write_is_rejected_without_truncation() {
        let chain = Arc::new(chain());
        let factory = create_test_provider_factory_with_node_types::<crate::BaseNode>(chain.clone());
        let static_files = factory.static_file_provider();
        let mut writer = static_files.latest_writer(StaticFileSegment::Headers).unwrap();
        writer.append_header(chain.genesis_header(), &chain.genesis_hash()).unwrap(); writer.commit().unwrap(); drop(writer);
        let jar = static_files.get_segment_provider_for_block(StaticFileSegment::Headers, 0, None).unwrap();
        let path = jar.data_path().to_path_buf(); drop(jar);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap(); file.write_all(b"interrupted").unwrap(); file.sync_all().unwrap(); drop(file);
        let corrupted_size = fs::metadata(&path).unwrap().len();
        let admission = DeferredStateRootAdmission { service: Arc::new(PostStateRootService { activation: Some(10), ..Default::default() }), marker: PathBuf::new(), snapshot: None };
        assert!(admission.verify_before_recovery(&factory).is_err(), "an interrupted static append must reject before writable recovery");
        assert_eq!(fs::metadata(path).unwrap().len(), corrupted_size, "admission must leave every interrupted byte intact rather than healing/truncating it");
    }

    /// Long-range sync and deep unwind must stop before a staged pipeline can start.
    #[test]
    fn backfill_and_deep_unwind_refused_before_stage_execution() {
        for target in [reth_stages_types::PipelineTarget::Sync(B256::repeat_byte(1)), reth_stages_types::PipelineTarget::Unwind(0)] {
            let mut backfill: DeferredStateRootBackfill<reth_node_builder::NodeTypesWithDBAdapter<crate::BaseNode, reth_db::DatabaseEnv>> = DeferredStateRootBackfill::Refuse { requested: false };
            let mut context = Context::from_waker(std::task::Waker::noop());
            assert!(backfill.poll(&mut context).is_pending(), "experimental pipeline must be idle before a fallback request");
            backfill.on_action(BackfillAction::Start(target));
            let Poll::Ready(BackfillEvent::TaskDropped(message)) = backfill.poll(&mut context) else { panic!("fallback must return exactly a fatal task event, never Started or Finished"); };
            assert_eq!(message, "DeferredStateRoot forbids staged/snap/backfill/deep-reorg fallback", "both forward and unwind fallback paths must fail before stage execution");
        }
    }

    /// Metadata healing must not hide a safe/finalized pointer beyond the disposable snapshot.
    #[test]
    fn inconsistent_chain_metadata_rejected_before_recovery() {
        let factory = create_test_provider_factory_with_node_types::<crate::BaseNode>(Arc::new(chain()));
        let writer = factory.provider_rw().unwrap(); writer.save_safe_block_number(1).unwrap(); writer.commit().unwrap();
        let admission = DeferredStateRootAdmission { service: Arc::new(PostStateRootService { activation: Some(10), ..Default::default() }), marker: PathBuf::new(), snapshot: None };
        assert_eq!(admission.verify_before_recovery(&factory).unwrap_err().to_string(), "experimental snapshot safe/finalized metadata exceeds its tip", "uncertain chain metadata must reject before stock healing writes");
    }

    /// Checkpoint disagreement is rejected by our recovery hook before stock healing is invoked.
    #[test]
    fn inconsistent_checkpoint_rejected_before_recovery() {
        let factory = create_test_provider_factory_with_node_types::<crate::BaseNode>(Arc::new(chain()));
        let writer = factory.provider_rw().unwrap(); writer.save_stage_checkpoint(StageId::Execution, StageCheckpoint::new(1)).unwrap(); writer.commit().unwrap();
        let admission = DeferredStateRootAdmission { service: Arc::new(PostStateRootService { activation: Some(10), ..Default::default() }), marker: PathBuf::new(), snapshot: None };
        let error = admission.verify_before_recovery(&factory).unwrap_err();
        assert!(error.to_string().contains("inconsistent stage checkpoint Execution"), "checkpoint uncertainty must reject before any stock healing: {error}");
    }
}
