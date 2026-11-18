//! Answers trie requests: applies each block to every reth and Base state root path over its
//! own on-disk database and reports what each computed.

use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    panic::{self, AssertUnwindSafe},
    path::Path,
};

use alloy_primitives::B256;
use alloy_trie::EMPTY_ROOT_HASH;
use base_diff_fuzz::{COVERAGE_ENV, TrieBlock, TrieBlockResult, TrieRequest, TrieResponse};
use base_execution_trie::{MdbxProofsStorage, RocksdbProofsStorage};
use reth_db_api::models::StorageSettings;
use reth_trie_common::{
    ExecutionWitnessMode, HashedPostState, HashedPostStateSorted,
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
        let mut engine_sparse = EngineSparse::new(true, ArenaParallelismThresholds::default());
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
            variants.entry("witness").or_default().push(|| {
                Ok(root_only(serial.witness_root(
                    parent_root,
                    state,
                    ExecutionWitnessMode::Legacy,
                )?))
            });
            variants.entry("sparse-fresh").or_default().push(|| {
                Ok(root_only(fresh_sparse.root(&serial, number, parent_root, &changes.chunks)?.0))
            });
            variants.entry("sparse-eager").or_default().push(|| {
                Ok(root_only(eager_sparse.root(&serial, number, parent_root, &changes.chunks)?.0))
            });
            if let Err(err) = VariantResults::catch(|| serial.commit(state, updates)) {
                primary.push(failed(err));
                break;
            }
            variants.entry("full-rebuild").or_default().push(|| Ok(root_only(serial.full_root()?)));
            variants
                .entry("serial-tables")
                .or_default()
                .push(|| Ok(TrieBlockResult { error: serial.verify()?, ..Default::default() }));
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
                let (root, updates) =
                    engine_sparse.root(&engine, number, parent_root, &changes.chunks)?;
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
