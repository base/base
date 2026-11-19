//! Answers trie requests: applies each block to every reth and Base state root path over its
//! own on-disk database and reports what each computed.

use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    panic::{self, AssertUnwindSafe},
    path::Path,
    sync::Arc,
};

use alloy_primitives::B256;
use alloy_trie::EMPTY_ROOT_HASH;
use base_diff_fuzz::{COVERAGE_ENV, TrieBlock, TrieBlockResult, TrieRequest, TrieResponse};
use base_execution_trie::{MdbxProofsStorage, RocksdbProofsStorage};
use reth_db_api::models::StorageSettings;
use reth_trie_common::{
    ExecutionWitnessMode, HashedPostState, HashedPostStateSorted, TrieInputSorted,
    updates::{TrieUpdates, TrieUpdatesSorted},
};
use reth_trie_sparse::ArenaParallelismThresholds;

use crate::{BlockChanges, EngineSparse, ProofsHistory, RethDb};

/// Results of one implementation, which stops at its first error.
#[derive(Debug, Default)]
pub struct VariantResults {
    /// One result per block.
    pub blocks: Vec<TrieBlockResult>,
    /// Whether an earlier block failed.
    pub failed: bool,
}

impl VariantResults {
    /// Records the result of `compute`, or its error or panic; after one, records empty results.
    pub fn push(&mut self, compute: impl FnOnce() -> eyre::Result<TrieBlockResult>) {
        if self.failed {
            self.blocks.push(TrieBlockResult::default());
            return;
        }
        let result = Self::catch(compute).unwrap_or_else(|err| {
            self.failed = true;
            TrieBlockResult { error: Some(format!("{err:#}")), ..Default::default() }
        });
        self.blocks.push(result);
    }

    /// Runs `compute`, turning a panic into an error.
    pub fn catch<T>(compute: impl FnOnce() -> eyre::Result<T>) -> eyre::Result<T> {
        panic::catch_unwind(AssertUnwindSafe(compute)).unwrap_or_else(|payload| {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default();
            Err(eyre::eyre!("panicked: {message}"))
        })
    }
}

/// A block the overlay database holds in memory, awaiting persistence.
#[derive(Debug)]
pub struct MemoryBlock {
    /// Trie updates, sorted for the overlay.
    pub nodes: TrieUpdatesSorted,
    /// Hashed state, sorted for the overlay.
    pub sorted: HashedPostStateSorted,
    /// Hashed state to persist.
    pub state: HashedPostState,
    /// Trie updates to persist.
    pub updates: TrieUpdates,
    /// Whether the block was applied with its unwind's revert ops on top of the blocks below it,
    /// so the block below it is not its parent.
    pub stacked: bool,
}

/// The base/base trie adapter.
#[derive(Debug)]
pub struct TrieAdapter {
    /// Task runtime the provider factories share.
    pub runtime: reth_tasks::Runtime,
    /// Whether to report edge coverage.
    pub coverage: bool,
}

impl TrieAdapter {
    /// Blocks the overlay database keeps in memory before persisting the oldest.
    pub const MEMORY_BLOCKS: usize = 2;

    /// An adapter reporting coverage when `BASE_DIFF_COVERAGE` asks for it.
    pub fn new() -> Self {
        let coverage = std::env::var(COVERAGE_ENV).is_ok_and(|v| !v.is_empty() && v != "0");
        Self { runtime: reth_tasks::Runtime::test(), coverage }
    }

    /// Answers one request line.
    pub fn handle_line(&self, line: &str) -> TrieResponse {
        let request: TrieRequest = match serde_json::from_str(line) {
            Ok(request) => request,
            Err(err) => {
                let id = serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|v| v.get("id")?.as_str().map(str::to_owned))
                    .unwrap_or_default();
                return TrieResponse {
                    id,
                    error: Some(format!("unparseable request: {err}")),
                    ..Default::default()
                };
            }
        };
        #[cfg(feature = "edge-coverage")]
        base_diff_adapter::EdgeCounters::reset();
        let dir = Path::new(&request.trie.dir);
        let result = fs::create_dir_all(dir)
            .map_err(eyre::Report::from)
            .and_then(|()| self.run(&request.trie.blocks, dir));
        let _ = fs::remove_dir_all(dir);
        let mut response = match result {
            Ok((blocks, variants)) => {
                TrieResponse { id: request.id, ok: true, blocks, variants, ..Default::default() }
            }
            Err(err) => TrieResponse {
                id: request.id,
                error: Some(format!("{err:#}")),
                ..Default::default()
            },
        };
        if self.coverage {
            #[cfg(feature = "edge-coverage")]
            {
                response.coverage =
                    Some(base_diff_adapter::EdgeCounters::with_edges(BTreeMap::new()));
            }
            #[cfg(not(feature = "edge-coverage"))]
            {
                response.coverage = Some(BTreeMap::new());
            }
        }
        response
    }

    /// Applies `blocks` and returns the primary results and every variant's.
    #[allow(clippy::type_complexity)]
    pub fn run(
        &self,
        blocks: &[TrieBlock],
        dir: &Path,
    ) -> eyre::Result<(Vec<TrieBlockResult>, BTreeMap<String, Vec<TrieBlockResult>>)> {
        let serial =
            RethDb::open(&dir.join("serial"), StorageSettings::v1(), self.runtime.clone())?;
        let engine =
            RethDb::open(&dir.join("engine"), StorageSettings::v2(), self.runtime.clone())?;
        let overlay =
            RethDb::open(&dir.join("overlay"), StorageSettings::v2(), self.runtime.clone())?;
        let mut ph_mdbx = ProofsHistory::new(MdbxProofsStorage::new(&dir.join("ph-mdbx"))?)?;
        let mut ph_rocksdb =
            ProofsHistory::new(RocksdbProofsStorage::new(&dir.join("ph-rocksdb"))?)?;
        // The pruned stores keep enough blocks below the highest tip for every unwind, as a
        // node's proofs window covers its reorg depth.
        let mut retention = 1;
        let (mut tip, mut highest) = (0, 0);
        for block in blocks {
            tip -= block.unwind;
            retention = retention.max(highest - tip);
            tip += 1;
            highest = highest.max(tip);
        }
        // Proofs by address and slot key need every proof target's preimages.
        let preimages = blocks.iter().flat_map(|b| &b.proofs).all(|target| {
            target.address.is_some() && (target.slots.is_empty() || target.slot_keys.is_some())
        });
        // Stores backfilled from the reth databases once no later block unwinds below them,
        // then fed live.
        let init_index = blocks.iter().rposition(|b| b.unwind > 0).unwrap_or(0);
        let mut ph_init: Option<(
            ProofsHistory<MdbxProofsStorage>,
            ProofsHistory<RocksdbProofsStorage>,
        )> = None;
        let mut ph_mdbx_pruned =
            ProofsHistory::new(MdbxProofsStorage::new(&dir.join("ph-mdbx-pruned"))?)?;
        let mut ph_rocksdb_pruned =
            ProofsHistory::new(RocksdbProofsStorage::new(&dir.join("ph-rocksdb-pruned"))?)?;
        let mut engine_sparse = EngineSparse::new(true, ArenaParallelismThresholds::default())
            .with_workers(self.runtime.clone());
        let mut eager_sparse = EngineSparse::new(true, EngineSparse::eager());
        let mut fresh_sparse = EngineSparse::new(false, ArenaParallelismThresholds::default());
        let mut memory: VecDeque<MemoryBlock> = VecDeque::new();
        let mut native = true;

        let mut primary = Vec::with_capacity(blocks.len());
        let mut variants: BTreeMap<&str, VariantResults> = BTreeMap::new();
        let mut canonical: Vec<usize> = Vec::new();
        let mut states = Vec::with_capacity(blocks.len());
        let mut parent_root = EMPTY_ROOT_HASH;
        for (index, block) in blocks.iter().enumerate() {
            if block.unwind > 0 {
                engine_sparse.forget();
                eager_sparse.forget();
                canonical.truncate(canonical.len().saturating_sub(block.unwind as usize));
                // In-memory blocks unwind natively when each dropped block sits on its parent.
                let unwind = block.unwind as usize;
                native =
                    unwind <= memory.len() && memory.iter().rev().take(unwind).all(|b| !b.stacked);
                if native {
                    memory.truncate(memory.len() - unwind);
                }
            }
            canonical.push(index);
            let changes = BlockChanges::new(
                block.revert.iter().chain(&block.ops),
                |key| serial.slots(key).unwrap_or_default(),
                |key| serial.account(key).ok().flatten(),
            );
            let state = &changes.state;
            let number = block.number;
            let root_only = |root: B256| TrieBlockResult {
                number: Some(number),
                state_root: Some(root),
                ..Default::default()
            };

            // Paths over the serial database at the parent state.
            let failed = |err: eyre::Report| TrieBlockResult {
                number: Some(number),
                error: Some(format!("{err:#}")),
                ..Default::default()
            };
            let (root, updates) = match VariantResults::catch(|| serial.serial_root(state)) {
                Ok(computed) => computed,
                Err(err) => {
                    primary.push(failed(err));
                    break;
                }
            };
            variants
                .entry("serial-root")
                .or_default()
                .push(|| Ok(root_only(serial.factory.latest()?.state_root(state.clone())?)));
            variants.entry("witness").or_default().push(|| {
                Ok(root_only(serial.witness_root(
                    parent_root,
                    state,
                    ExecutionWitnessMode::Legacy,
                )?))
            });
            variants.entry("sparse-fresh").or_default().push(|| {
                Ok(root_only(
                    fresh_sparse
                        .root(
                            &serial,
                            number,
                            parent_root,
                            &changes.chunks,
                            &TrieInputSorted::default(),
                        )?
                        .0,
                ))
            });
            // After a reorg below the persisted tip, the overlay reverts the database's newer
            // state with the overlay's keys as prefix sets, since the stored nodes describe
            // the tip.
            variants.entry("sparse-reverts").or_default().push(|| {
                if block.unwind == 0 {
                    return Ok(TrieBlockResult::default());
                }
                let reverts = BlockChanges::new(
                    block.revert.iter(),
                    |key| serial.slots(key).unwrap_or_default(),
                    |key| serial.account(key).ok().flatten(),
                )
                .state
                .into_sorted();
                let prefix_sets = reverts.construct_prefix_sets();
                let overlay = TrieInputSorted::new(Arc::default(), Arc::new(reverts), prefix_sets);
                let (root, _) = EngineSparse::new(false, ArenaParallelismThresholds::default())
                    .root(&serial, number, parent_root, &changes.chunks, &overlay)?;
                Ok(root_only(root))
            });
            variants.entry("sparse-eager").or_default().push(|| {
                Ok(root_only(
                    eager_sparse
                        .root(
                            &serial,
                            number,
                            parent_root,
                            &changes.chunks,
                            &TrieInputSorted::default(),
                        )?
                        .0,
                ))
            });
            if let Err(err) = VariantResults::catch(|| serial.commit(state, updates)) {
                primary.push(failed(err));
                break;
            }
            variants
                .entry("merkle-rebuild")
                .or_default()
                .push(|| Ok(root_only(serial.rebuild_root_with_progress(1 + number % 7)?)));
            variants.entry("full-rebuild").or_default().push(|| Ok(root_only(serial.full_root()?)));
            variants
                .entry("serial-tables")
                .or_default()
                .push(|| Ok(TrieBlockResult { error: serial.verify()?, ..Default::default() }));
            if preimages {
                variants
                    .entry("storage-api")
                    .or_default()
                    .push(|| RethDb::storage_api_proofs(&serial.factory.latest()?, &block.proofs));
                variants.entry("getproof").or_default().push(|| {
                    let proofs = RethDb::account_proofs(&serial.factory.latest()?, &block.proofs)?;
                    Ok(TrieBlockResult { proofs: Some(proofs), ..Default::default() })
                });
            }
            let exists: Vec<B256> = changes
                .written
                .iter()
                .filter(|key| serial.account(key).ok().flatten().is_some())
                .copied()
                .collect();
            let reads = VariantResults::catch(|| {
                Ok((serial.storage_roots(&exists)?, serial.proofs(&block.proofs)?))
            });
            match reads {
                Ok((storage_roots, proofs)) => primary.push(TrieBlockResult {
                    number: Some(number),
                    state_root: Some(root),
                    storage_roots: Some(storage_roots.into_iter().collect()),
                    proofs: Some(proofs),
                    error: None,
                }),
                Err(err) => {
                    primary.push(failed(err));
                    break;
                }
            }

            // The engine database: the sparse trie's root and trie updates are persisted.
            variants
                .entry("engine-serial")
                .or_default()
                .push(|| Ok(root_only(engine.serial_root(state)?.0)));
            variants.entry("engine").or_default().push(|| {
                let (root, updates) = engine_sparse.root(
                    &engine,
                    number,
                    parent_root,
                    &changes.chunks,
                    &TrieInputSorted::default(),
                )?;
                engine.commit(state, updates)?;
                Ok(TrieBlockResult {
                    number: Some(number),
                    state_root: Some(root),
                    storage_roots: Some(engine.storage_roots(&exists)?.into_iter().collect()),
                    proofs: Some(engine.proofs(&block.proofs)?),
                    error: None,
                })
            });
            variants
                .entry("engine-tables")
                .or_default()
                .push(|| Ok(TrieBlockResult { error: engine.verify()?, ..Default::default() }));

            // The overlay database: recent blocks stay in memory above the persisted ones.
            variants.entry("overlay-sparse").or_default().push(|| {
                let mut nodes = TrieUpdatesSorted::default();
                let mut hashed = HashedPostStateSorted::default();
                for memory_block in &memory {
                    nodes.extend_ref_and_sort(&memory_block.nodes);
                    hashed.extend_ref_and_sort(&memory_block.sorted);
                }
                let overlay_input =
                    TrieInputSorted::new(Arc::new(nodes), Arc::new(hashed), Default::default());
                let (root, _) = EngineSparse::new(false, ArenaParallelismThresholds::default())
                    .root(&overlay, number, parent_root, &changes.chunks, &overlay_input)?;
                Ok(root_only(root))
            });
            variants.entry("overlay").or_default().push(|| {
                let sorted: Vec<_> =
                    memory.iter().map(|b| (b.nodes.clone(), b.sorted.clone())).collect();
                let (root, updates) = overlay.overlay_root(&sorted, state)?;
                memory.push_back(MemoryBlock {
                    nodes: updates.clone().into_sorted(),
                    sorted: state.clone().into_sorted(),
                    state: state.clone(),
                    updates,
                    stacked: block.unwind > 0 && !native,
                });
                while memory.len() > Self::MEMORY_BLOCKS {
                    let oldest = memory.pop_front().expect("nonempty");
                    overlay.commit(&oldest.state, oldest.updates)?;
                }
                Ok(root_only(root))
            });

            // Base's proofs-history stores.
            variants
                .entry("ph-mdbx")
                .or_default()
                .push(|| ph_mdbx.block(number, block.unwind, state, &block.proofs));
            variants
                .entry("ph-rocksdb")
                .or_default()
                .push(|| ph_rocksdb.block(number, block.unwind, state, &block.proofs));
            variants
                .entry("ph-mdbx-witness")
                .or_default()
                .push(|| Ok(root_only(ph_mdbx.witness_root(number - 1, state)?)));
            if preimages {
                variants
                    .entry("ph-rocksdb-storage-api")
                    .or_default()
                    .push(|| ph_rocksdb.storage_api_proofs(number, &block.proofs));
                variants.entry("ph-mdbx-getproof").or_default().push(|| {
                    Ok(TrieBlockResult {
                        proofs: Some(ph_mdbx.account_proofs(number, &block.proofs)?),
                        ..Default::default()
                    })
                });
            }
            variants.entry("ph-mdbx-pruned").or_default().push(|| {
                let result = ph_mdbx_pruned.block(number, block.unwind, state, &block.proofs)?;
                ph_mdbx_pruned.prune(retention, 1 + number % 2);
                Ok(result)
            });
            variants.entry("ph-rocksdb-pruned").or_default().push(|| {
                let result = ph_rocksdb_pruned.block(number, block.unwind, state, &block.proofs)?;
                // The RocksDB store prunes every third block, one block per transaction, so one
                // run prunes in several batches, as the prune task does after falling behind.
                if number % 3 == 0 {
                    ph_rocksdb_pruned.prune(retention, 1);
                }
                Ok(result)
            });

            let init_results = VariantResults::catch(|| {
                if index < init_index {
                    return Ok(None);
                }
                if let Some((mdbx, rocksdb)) = &mut ph_init {
                    return Ok(Some((
                        mdbx.block(number, 0, state, &block.proofs)?,
                        rocksdb.block(number, 0, state, &block.proofs)?,
                    )));
                }
                let mdbx = ProofsHistory::initialized(
                    MdbxProofsStorage::new(&dir.join("ph-init-mdbx"))?,
                    &serial,
                    number,
                    true,
                )?;
                let rocksdb = ProofsHistory::initialized(
                    RocksdbProofsStorage::new(&dir.join("ph-init-rocksdb"))?,
                    &engine,
                    number,
                    false,
                )?;
                let results =
                    (mdbx.read(number, &block.proofs)?, rocksdb.read(number, &block.proofs)?);
                ph_init = Some((mdbx, rocksdb));
                Ok(Some(results))
            });
            for (name, pick) in [("ph-init-mdbx", 0), ("ph-init-rocksdb", 1)] {
                let result = match &init_results {
                    Ok(None) => Ok(TrieBlockResult::default()),
                    Ok(Some(pair)) => Ok(if pick == 0 { pair.0.clone() } else { pair.1.clone() }),
                    Err(err) => Err(eyre::eyre!("{err:#}")),
                };
                variants.entry(name).or_default().push(|| result);
            }
            states.push((number, changes.state, block.proofs.clone()));
            parent_root = root;
        }

        // After a failure of the primary path, the remaining blocks have no results.
        primary.resize(blocks.len(), TrieBlockResult::default());
        let mut variants: BTreeMap<String, Vec<TrieBlockResult>> = variants
            .into_iter()
            .map(|(name, mut results)| {
                results.blocks.resize(blocks.len(), TrieBlockResult::default());
                (name.to_owned(), results.blocks)
            })
            .collect();
        let reads: Vec<_> = states.iter().map(|(n, _, proofs)| (*n, proofs.clone())).collect();
        let is_canonical: Vec<bool> = (0..blocks.len()).map(|i| canonical.contains(&i)).collect();
        for (name, history) in [
            ("ph-mdbx-history", ph_mdbx.history(&reads, &is_canonical)),
            ("ph-rocksdb-history", ph_rocksdb.history(&reads, &is_canonical)),
            ("ph-mdbx-pruned-history", ph_mdbx_pruned.history(&reads, &is_canonical)),
            ("ph-rocksdb-pruned-history", ph_rocksdb_pruned.history(&reads, &is_canonical)),
        ] {
            let mut history = history;
            history.resize(blocks.len(), TrieBlockResult::default());
            variants.insert(name.to_owned(), history);
        }
        if blocks.iter().all(|b| b.unwind == 0) {
            let store = MdbxProofsStorage::new(&dir.join("ph-batch"))?;
            let mut batch = ProofsHistory::batch(store, &states)?;
            batch.resize(blocks.len(), TrieBlockResult::default());
            variants.insert("ph-batch".to_owned(), batch);
        }
        Ok((primary, variants))
    }
}

impl Default for TrieAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use base_diff_fuzz::{Rng, TrieCompare, TrieGen};

    use super::*;

    #[test]
    fn every_path_agrees_with_the_model_on_generated_cases() {
        let adapter = TrieAdapter::new();
        for seed in 0..4 {
            let case = TrieGen::new(Rng::new(&[seed])).case();
            let dir =
                std::env::temp_dir().join(format!("trie-adapter-{}-{seed}", std::process::id()));
            let built = case.build(seed.to_string(), dir.display().to_string());
            let line = serde_json::to_string(&built.request).unwrap();
            let response = adapter.handle_line(&line);
            assert!(response.ok, "{:?}", response.error);
            assert!(!dir.exists());
            let mut results = vec![
                ("model".to_owned(), Ok(built.expected)),
                ("base".to_owned(), Ok(response.blocks)),
            ];
            results.extend(response.variants.into_iter().map(|(name, list)| (name, Ok(list))));
            let verdict = TrieCompare::compare(case.blocks.len(), &results);
            assert!(
                verdict.agrees(),
                "seed {seed}: {:#?}",
                &verdict.differences[..verdict.differences.len().min(5)]
            );
        }
    }
}
