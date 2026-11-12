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
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_engine_tree::backfill::{BackfillAction, BackfillEvent, BackfillSync, PipelineSync};
use reth_node_builder::{NodeConfig, sync::{BackfillContext, BackfillSyncBuilder, PipelineBackfill}};
use reth_provider::{BlockNumReader, HeaderProvider, LatestStateProviderRef, MetadataProvider, ProviderFactory, StageCheckpointReader, StateRootProvider, StaticFileProviderFactory, StaticFileSegment, providers::ProviderNodeTypes};
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
        let activation = config.chain.validate_deferred_state_root_schedule()?;
        let cohort = args.root_experiment_cohort.unwrap_or(if activation.is_some() { PostStateRootCohort::Deferred } else { PostStateRootCohort::Ordinary });
        if activation.is_none() && cohort == PostStateRootCohort::Ordinary { return Ok(None); }
        ensure!(activation.is_some() == (cohort == PostStateRootCohort::Deferred), "DeferredStateRoot schedule must select deferred cohort; serial controls require an unscheduled custom chain");
        ensure!(!matches!(config.chain.chain().id(), 8453 | 84532), "root experiments forbid mainnet and Sepolia");
        ensure!(args.root_experiment_queue > 0 && args.root_experiment_ready >= 2 && args.root_experiment_timeout_ms > 0, "root experiment limits require positive queue/deadline and at least two ready entries");
        ensure!(args.deferred_state_root_disposable, "DeferredStateRoot requires --rollup.deferred-state-root-disposable");
        ensure!(!config.debug.skip_state_root, "DeferredStateRoot is incompatible with debug.skip-state-root");
        ensure!(config.debug.invalid_block_hook.as_ref().is_none_or(|hooks| hooks.is_empty()), "DeferredStateRoot is incompatible with configured invalid-block diagnostic hooks, including witness");
        ensure!(!args.proofs_history && args.proofs_history_storage_path.is_none(), "DeferredStateRoot forbids proof history services");
        ensure!(args.upgrade_signal.contract_address.is_none() && args.upgrade_signal_l1_rpc.upgrade_signal_l1_rpc.is_none(), "DeferredStateRoot forbids L1 upgrade-signal RPC");
        ensure!(args.sequencer.is_none(), "DeferredStateRoot forbids production sequencer forwarding");
        ensure!(extension_count == 0, "DeferredStateRoot forbids unverified node extensions and production proof/output/L1 services");
        let datadir = config.datadir();
        ensure!(datadir.static_files() == datadir.data_dir().join("static_files") && datadir.rocksdb() == datadir.data_dir().join("rocksdb"), "DeferredStateRoot requires all stores under the disposable datadir");
        let marker = Self::marker_path(datadir.data_dir())?;
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

    /// Rejects active/unmarked or inconsistent checkpoint state before stock multi-store recovery.
    /// Does not call ProviderFactory::check_consistency, which can heal/write storage.
    pub fn verify_before_recovery<N>(&self, factory: &ProviderFactory<N>) -> Result<()>
    where N: ProviderNodeTypes<Primitives = BasePrimitives, ChainSpec = BaseChainSpec>,
    {
        let provider = factory.provider()?;
        ensure!(provider.pipeline_unwind()?.is_none() && provider.snap_attempt()?.is_none(), "experimental store has pending unwind or snap recovery");
        ensure!(provider.get_metadata("partial_state_trie_unwind")?.is_none(), "experimental store has partial-trie unwind recovery");
        let tip = provider.last_block_number()?;
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
        for segment in [StaticFileSegment::Headers, StaticFileSegment::Transactions, StaticFileSegment::Receipts, StaticFileSegment::AccountChangeSets, StaticFileSegment::StorageChangeSets] {
            ensure!(static_files.get_highest_static_file_block(segment).is_none_or(|height| height <= tip), "experimental snapshot has static files ahead of its checkpoint");
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
    use reth_provider::{test_utils::create_test_provider_factory_with_node_types, StageCheckpointWriter};
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
