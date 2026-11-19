//! The engine's sparse trie state root: proofs revealed from the database through the v2
//! multiproof calculator or the engine's proof workers, transaction updates applied as leaf
//! updates, and the trie kept across blocks.

use std::sync::Arc;

use alloy_primitives::{B256, map::B256Map};
use alloy_rlp::{Decodable, Encodable};
use alloy_trie::{EMPTY_ROOT_HASH, TrieAccount};
use reth_primitives_traits::Account;
use reth_provider::{
    DatabaseProviderFactory, ProviderResult, StateProviderBox, StorageSettingsCache,
};
use reth_storage_api::DatabaseProviderROFactory;
use reth_trie::{
    hashed_cursor::HashedPostStateCursorFactory, proof_v2::MultiProofCalculator,
    trie_cursor::InMemoryTrieCursorFactory,
};
use reth_trie_common::{
    DecodedMultiProofV2, HashedPostState, MultiProofTargetsV2, ProofV2Target, TrieInputSorted,
    updates::TrieUpdates,
};
use reth_trie_db::{DatabaseHashedCursorFactory, DatabaseTrieCursorFactory};
use reth_trie_parallel::proof_task::{
    ProofTaskCtx, ProofWorkerCounts, ProofWorkerHandle, metrics::ProofWorkerIdleMetrics,
};
use reth_trie_sparse::{
    ArenaParallelSparseTrie, ArenaParallelismThresholds, LeafUpdate, RevealableSparseTrie,
    SparseStateTrie, TrieNodeEpoch,
};

use crate::{RethDb, RethFactory};

/// A sparse state trie driven as the engine's state root task drives it.
///
/// For each transaction-sized chunk of a block, storage slots become leaf updates of their
/// storage tries and accounts become leaf updates of the account trie, with proofs fetched from
/// the database for every blinded path an update reaches. With `reuse`, the trie and its storage
/// roots carry over to the next block, as the engine preserves them for a child of the block it
/// just validated, and the proof calculator takes those roots instead of reading storage tries.
#[derive(Debug)]
pub struct EngineSparse {
    /// Keep the trie across blocks.
    pub reuse: bool,
    /// Parallelism thresholds of the arena tries.
    pub thresholds: ArenaParallelismThresholds,
    /// The trie kept from the previous block, with its storage roots.
    pub kept: Option<(SparseStateTrie, B256Map<B256>)>,
    /// Serve proofs through the engine's proof worker pools on this runtime, instead of one
    /// calculator on the caller's thread.
    pub workers: Option<reth_tasks::Runtime>,
}

/// Where a sparse trie gets the proofs of the paths it reports blinded.
pub trait ProofSource {
    /// The decoded multiproofs of `targets`.
    fn prove(&mut self, targets: MultiProofTargetsV2) -> eyre::Result<Vec<DecodedMultiProofV2>>;
}

impl<T, H> ProofSource for MultiProofCalculator<'_, T, H>
where
    T: reth_trie::trie_cursor::TrieCursorFactory,
    H: reth_trie::hashed_cursor::HashedCursorFactory,
{
    fn prove(&mut self, targets: MultiProofTargetsV2) -> eyre::Result<Vec<DecodedMultiProofV2>> {
        Ok(vec![self.multiproof(targets)?])
    }
}

impl ProofSource for ProofWorkerHandle {
    /// Queues the targets as the engine does (one storage job per account, one account job),
    /// chunked as its sparse trie task chunks them (`dispatch_with_chunking`, with the default
    /// chunk size of 5, and always above 300 targets), and waits for every job's result.
    fn prove(&mut self, targets: MultiProofTargetsV2) -> eyre::Result<Vec<DecodedMultiProofV2>> {
        const CHUNK_SIZE: usize = 5;
        const MAX_TARGETS_FOR_CHUNKING: usize = 300;
        let len = targets.chunking_length();
        let idle =
            self.has_multiple_idle_account_workers() || self.has_multiple_idle_storage_workers();
        let chunked =
            (len > MAX_TARGETS_FOR_CHUNKING || (len >= 2 * CHUNK_SIZE && idle)) && len > CHUNK_SIZE;
        let chunks: Vec<MultiProofTargetsV2> =
            if chunked { targets.chunks(CHUNK_SIZE).collect() } else { vec![targets] };
        let mut jobs = 0;
        for chunk in chunks {
            jobs += self.dispatch(chunk)?;
        }
        (0..jobs).map(|_| Ok(self.results().recv()??)).collect()
    }
}

/// Proof providers over the latest database state, for the proof workers: each worker opens one
/// per block, as the engine's workers open overlay providers.
#[derive(Debug, Clone)]
pub struct LatestProofs(pub RethFactory);

impl DatabaseProviderROFactory for LatestProofs {
    type Provider = StateProviderBox;

    fn database_provider_ro(&self) -> ProviderResult<Self::Provider> {
        self.0.latest()
    }
}

impl EngineSparse {
    /// A sparse root computation; see the type docs.
    pub const fn new(reuse: bool, thresholds: ArenaParallelismThresholds) -> Self {
        Self { reuse, thresholds, kept: None, workers: None }
    }

    /// Serves proofs through the engine's proof workers on `runtime`.
    pub fn with_workers(mut self, runtime: reth_tasks::Runtime) -> Self {
        self.workers = Some(runtime);
        self
    }

    /// Thresholds of one, which send even tiny tries down the parallel code paths.
    pub const fn eager() -> ArenaParallelismThresholds {
        ArenaParallelismThresholds {
            min_dirty_leaves: 1,
            min_revealed_nodes: 1,
            min_updates: 1,
            min_leaves_for_prune: 1,
        }
    }

    fn fresh(&self) -> SparseStateTrie {
        let blank = RevealableSparseTrie::blind_from(
            ArenaParallelSparseTrie::default().with_parallelism_thresholds(self.thresholds),
        );
        SparseStateTrie::default()
            .with_accounts_trie(blank.clone())
            .with_default_storage_trie(blank)
            .with_updates(true)
    }

    /// Forgets the kept trie, as after a reorg: the engine only reuses a trie for a child of
    /// the block it was computed for.
    pub fn forget(&mut self) {
        self.kept = None;
    }

    /// The state root and trie updates of block `number` (with parent root `parent_root`) over
    /// `db` with `overlay` on top, which together hold the parent state. Proofs come from the
    /// calculator the overlay state provider builds: in-memory trie nodes and hashed state over
    /// the database's, with the overlay's prefix sets marking stored nodes that are stale.
    pub fn root(
        &mut self,
        db: &RethDb,
        number: u64,
        parent_root: B256,
        chunks: &[HashedPostState],
        overlay: &TrieInputSorted,
    ) -> eyre::Result<(B256, TrieUpdates)> {
        let epoch = TrieNodeEpoch::new(number);
        let (mut trie, known) = match self.kept.take() {
            Some(kept) if self.reuse => kept,
            _ => (self.fresh(), B256Map::default()),
        };
        if let Some(runtime) = &self.workers {
            let mut workers = ProofWorkerHandle::new(
                runtime,
                ProofTaskCtx::new(LatestProofs(db.factory.clone())),
                ProofWorkerCounts::new(2, 2),
                ProofWorkerIdleMetrics::default(),
            );
            for chunk in chunks {
                Self::apply(&mut trie, &mut workers, chunk, epoch)?;
            }
        } else {
            let provider = DatabaseProviderFactory::database_provider_ro(&db.factory)?;
            reth_trie_db::with_adapter!(provider, |A| {
                let mut calculator = MultiProofCalculator::new(
                    InMemoryTrieCursorFactory::new(
                        DatabaseTrieCursorFactory::<_, A>::new(provider.tx_ref()),
                        overlay.nodes.as_ref(),
                    ),
                    HashedPostStateCursorFactory::new(
                        DatabaseHashedCursorFactory::new(provider.tx_ref()),
                        overlay.state.as_ref(),
                    ),
                    overlay.prefix_sets.clone().freeze(),
                    Arc::new(known),
                );
                for chunk in chunks {
                    Self::apply(&mut trie, &mut calculator, chunk, epoch)?;
                }
                eyre::Ok(())
            })?;
        }
        let (root, updates) = if trie.trie_mut().as_revealed_ref().is_none() {
            (parent_root, TrieUpdates::default())
        } else {
            trie.root_with_updates(epoch)?
        };
        if self.reuse {
            let roots = trie.storage_roots(epoch);
            trie.prune(TrieNodeEpoch::new(number.saturating_sub(1)));
            self.kept = Some((trie, roots));
        }
        Ok((root, updates))
    }

    /// Reveals and updates until `updates` are all applied to one trie, fetching proofs for the
    /// paths each round reports blinded.
    fn drain(
        trie: &mut SparseStateTrie,
        proofs: &mut impl ProofSource,
        account: Option<B256>,
        updates: &mut B256Map<LeafUpdate>,
    ) -> eyre::Result<()> {
        while !updates.is_empty() {
            let mut targets = MultiProofTargetsV2::default();
            match account {
                Some(address) => {
                    trie.get_or_create_storage_trie_mut(address).update_leaves(
                        updates,
                        |key, parent| {
                            targets
                                .storage_targets
                                .entry(address)
                                .or_default()
                                .push(ProofV2Target::new(key).with_parent(parent));
                        },
                    )?;
                }
                None => trie.trie_mut().update_leaves(updates, |key, parent| {
                    targets.account_targets.push(ProofV2Target::new(key).with_parent(parent));
                })?,
            }
            if targets.is_empty() {
                eyre::ensure!(updates.is_empty(), "sparse trie kept updates without targets");
                break;
            }
            for multiproof in proofs.prove(targets)? {
                trie.reveal_decoded_multiproof_v2(multiproof)?;
            }
        }
        Ok(())
    }

    fn apply(
        trie: &mut SparseStateTrie,
        proofs: &mut impl ProofSource,
        chunk: &HashedPostState,
        epoch: TrieNodeEpoch,
    ) -> eyre::Result<()> {
        let mut addresses: Vec<B256> =
            chunk.accounts.keys().chain(chunk.storages.keys()).copied().collect();
        addresses.sort();
        addresses.dedup();
        // An account whose storage the chunk names without changing any slot has no storage
        // update, as in the engine.
        let changed =
            |address: &B256| chunk.storages.get(address).is_some_and(|s| !s.storage.is_empty());
        for (address, storage) in chunk.storages.iter().filter(|(a, _)| changed(a)) {
            let mut updates: B256Map<LeafUpdate> = storage
                .storage
                .iter()
                .map(|(slot, value)| {
                    let encoded = if value.is_zero() {
                        Vec::new()
                    } else {
                        alloy_rlp::encode_fixed_size(value).to_vec()
                    };
                    (*slot, LeafUpdate::Changed(encoded))
                })
                .collect();
            Self::drain(trie, proofs, Some(*address), &mut updates)?;
        }
        // Reveal every account first, then encode its leaf with its new storage root.
        let mut touched: B256Map<LeafUpdate> =
            addresses.iter().map(|a| (*a, LeafUpdate::Touched)).collect();
        Self::drain(trie, proofs, None, &mut touched)?;
        let mut updates = B256Map::default();
        for address in addresses {
            let current = trie
                .get_account_value(&address)
                .map(|value| TrieAccount::decode(&mut value.as_slice()))
                .transpose()?;
            let account =
                chunk.accounts.get(&address).map_or_else(|| current.map(Account::from), |a| *a);
            let storage_root = if changed(&address) {
                trie.storage_root(&address, epoch)
                    .ok_or_else(|| eyre::eyre!("storage trie of {address} is blind"))?
            } else {
                current.map_or(EMPTY_ROOT_HASH, |a| a.storage_root)
            };
            let encoded = if account.is_none_or(|a| a.is_empty()) && storage_root == EMPTY_ROOT_HASH
            {
                Vec::new()
            } else {
                let mut out = Vec::new();
                account.unwrap_or_default().into_trie_account(storage_root).encode(&mut out);
                out
            };
            updates.insert(address, LeafUpdate::Changed(encoded));
        }
        Self::drain(trie, proofs, None, &mut updates)
    }
}
