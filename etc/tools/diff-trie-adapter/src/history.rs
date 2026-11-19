//! Base's proofs-history store (`base-execution-trie`): versioned trie nodes and hashed state
//! per block, the overlay roots and proofs computed over them, unwinds, and historical reads.

use std::sync::{Arc, Mutex};

use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
use alloy_primitives::{B256, keccak256};
use base_diff_adapter::WitnessState;
use base_diff_fuzz::{ProofResult, ProofTarget, TrieBlockResult};
use base_execution_trie::{
    BaseProofStoragePruner, BaseProofsBatchSession, BaseProofsBatchStateProviderRef,
    BaseProofsBatchStore, BaseProofsInitialStateStore, BaseProofsStorage, BaseProofsStore,
    BlockStateDiff, InitializationJob, RethTrieStorageLayout, provider::BaseProofsStateProviderRef,
};
use reth_provider::{
    BlockHashReader, DatabaseProviderFactory, ProviderResult, StorageSettingsCache,
    noop::NoopProvider,
};
use reth_storage_api::{StateProofProvider, StateRootProvider};
use reth_trie_common::{ExecutionWitnessMode, HashedPostState, MultiProofTargets, TrieInput};

use crate::RethDb;

/// A proofs-history store fed block by block, as its live collector feeds it.
#[derive(Debug)]
pub struct ProofsHistory<S: BaseProofsStore + 'static> {
    /// The store.
    pub store: Arc<S>,
    /// The store, behind the node's (optionally metered) wrapper.
    pub storage: BaseProofsStorage<Arc<S>>,
    /// Hashes of the canonical blocks by number, from the anchor at block 0.
    pub chain: Vec<B256>,
    /// Blocks built so far, which makes hashes of reorged blocks unique.
    pub built: u64,
}

impl<S> ProofsHistory<S>
where
    S: BaseProofsStore + BaseProofsInitialStateStore + BaseProofsBatchStore + 'static,
    Arc<S>: BaseProofsStore + Clone,
{
    /// Anchors an empty store at block 0, the empty state.
    pub fn new(store: S) -> eyre::Result<Self> {
        let store = Arc::new(store);
        store.set_initial_state_anchor(BlockNumHash::new(0, B256::ZERO))?;
        store.commit_initial_state()?;
        Ok(Self { storage: Arc::clone(&store).into(), store, chain: vec![B256::ZERO], built: 0 })
    }

    /// A store backfilled from `db` at block `number`, as `base-proofs init` and the node's
    /// initialization job do when proofs history is enabled on a synced node.
    pub fn initialized(store: S, db: &RethDb, number: u64) -> eyre::Result<Self> {
        let store = Arc::new(store);
        let chain: Vec<B256> = (0..=number)
            .map(|n| keccak256([b"anchor".as_slice(), &n.to_be_bytes()].concat()))
            .collect();
        let storage: BaseProofsStorage<Arc<S>> = Arc::clone(&store).into();
        let layout = if db.factory.cached_storage_settings().is_v2() {
            RethTrieStorageLayout::Packed
        } else {
            RethTrieStorageLayout::Legacy
        };
        let tx = db.factory.database_provider_ro()?.into_tx();
        InitializationJob::new(storage.clone(), tx, layout).run(number, chain[number as usize])?;
        Ok(Self { storage, store, chain, built: 0 })
    }

    /// Roots and proofs at block `number`, read back from the store.
    pub fn read(&self, number: u64, targets: &[ProofTarget]) -> eyre::Result<TrieBlockResult> {
        self.proofs(number, targets)
    }

    /// The block reference of block `number` on top of the current chain.
    fn next_block(&mut self, number: u64) -> BlockWithParent {
        self.built += 1;
        let mut seed = [0u8; 16];
        seed[..8].copy_from_slice(&number.to_be_bytes());
        seed[8..].copy_from_slice(&self.built.to_be_bytes());
        BlockWithParent::new(self.chain[number as usize - 1], NumHash::new(number, keccak256(seed)))
    }

    /// Proofs of `targets` at block `number`.
    fn proofs(&self, number: u64, targets: &[ProofTarget]) -> eyre::Result<TrieBlockResult> {
        let provider =
            BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &self.storage, number);
        let root = provider.state_root(HashedPostState::default())?;
        let mut request = MultiProofTargets::default();
        for target in targets {
            request.entry(target.key).or_default().extend(target.slots.iter().copied());
        }
        let multiproof = provider.multiproof(TrieInput::default(), request)?;
        let proofs = targets
            .iter()
            .map(|target| RethDb::proof(&multiproof, target))
            .collect::<eyre::Result<_>>()?;
        Ok(TrieBlockResult {
            number: Some(number),
            state_root: Some(root),
            proofs: Some(proofs),
            ..Default::default()
        })
    }

    /// Proofs of `targets`, which all carry preimages, at block `number` as `eth_getProof` over
    /// the store computes them.
    pub fn account_proofs(
        &self,
        number: u64,
        targets: &[ProofTarget],
    ) -> eyre::Result<Vec<ProofResult>> {
        let provider =
            BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &self.storage, number);
        RethDb::account_proofs(&provider, targets)
    }

    /// [`RethDb::storage_api_proofs`] at block `number`.
    pub fn storage_api_proofs(
        &self,
        number: u64,
        targets: &[ProofTarget],
    ) -> eyre::Result<TrieBlockResult> {
        let provider =
            BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &self.storage, number);
        RethDb::storage_api_proofs(&provider, targets)
    }

    /// The state root of `state` on top of block `parent`, computed statelessly from the
    /// store's root and execution witness for `state` at `parent`.
    pub fn witness_root(&self, parent: u64, state: &HashedPostState) -> eyre::Result<B256> {
        let provider =
            BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &self.storage, parent);
        let parent_root = provider.state_root(HashedPostState::default())?;
        let nodes =
            provider.witness(TrieInput::default(), state.clone(), ExecutionWitnessMode::Legacy)?;
        Ok(WitnessState::new(parent_root, &nodes, &[], [], []).compute_state_root(state)?)
    }

    /// Builds block `number` (whose parent is `number - 1` after dropping `unwind` blocks) from
    /// `state` as the node's `ExEx` does: after a reorg, the dropped blocks unwound with
    /// `unwind_history`, then the block's root and trie updates over the store at the parent,
    /// stored, and the block's root and proofs read back.
    pub fn block(
        &mut self,
        number: u64,
        unwind: u64,
        state: &HashedPostState,
        targets: &[ProofTarget],
    ) -> eyre::Result<TrieBlockResult> {
        let parent = number - 1;
        if unwind > 0 {
            let first_dropped = BlockWithParent::new(
                self.chain[parent as usize],
                NumHash::new(number, self.chain[number as usize]),
            );
            self.storage.unwind_history(first_dropped)?;
        }
        self.chain.truncate(number as usize);
        let provider =
            BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &self.storage, parent);
        let (root, updates) = provider.state_root_with_updates(state.clone())?;
        drop(provider);
        let block = self.next_block(number);
        let diff = BlockStateDiff {
            sorted_trie_updates: updates.into_sorted(),
            sorted_post_state: state.clone().into_sorted(),
        };
        self.storage.store_trie_updates(block, diff)?;
        self.chain.push(block.block.hash);
        let mut result = self.proofs(number, targets)?;
        eyre::ensure!(
            result.state_root == Some(root),
            "root read back at block {number} differs from the overlay root {root}"
        );
        result.state_root = Some(root);
        Ok(result)
    }

    /// Runs the node's pruner over the store, keeping the last `retention` blocks and pruning at
    /// most `batch` blocks per transaction.
    pub fn prune(&self, retention: u64, batch: u64) {
        BaseProofStoragePruner::new(
            self.storage.clone(),
            ChainHashes(self.chain.clone()),
            retention,
            batch,
        )
        .run();
    }

    /// Roots and proofs read back after the last block at each of `blocks` (number and proof
    /// targets) that is still canonical and not pruned; the others stay empty.
    pub fn history(
        &self,
        blocks: &[(u64, Vec<ProofTarget>)],
        canonical: &[bool],
    ) -> Vec<TrieBlockResult> {
        let earliest = match self.storage.get_earliest_block_number() {
            Ok(earliest) => earliest.map_or(0, |(number, _)| number),
            Err(err) => {
                let error = Some(format!("earliest block: {err:#}"));
                return blocks
                    .iter()
                    .map(|_| TrieBlockResult { error: error.clone(), ..Default::default() })
                    .collect();
            }
        };
        blocks
            .iter()
            .zip(canonical)
            .map(|((number, targets), canonical)| {
                if !canonical || *number < earliest {
                    return TrieBlockResult::default();
                }
                self.proofs(*number, targets).unwrap_or_else(|err| TrieBlockResult {
                    error: Some(format!("history read at block {number}: {err:#}")),
                    ..Default::default()
                })
            })
            .collect()
    }

    /// Builds every block (none unwinds) in one batch session, as cold catch-up does: each
    /// block's root over the session's view of its parent, which includes the session's own
    /// earlier writes.
    pub fn batch(
        store: S,
        states: &[(u64, HashedPostState, Vec<ProofTarget>)],
    ) -> eyre::Result<Vec<TrieBlockResult>> {
        let mut history = Self::new(store)?;
        let blocks: Vec<BlockWithParent> = states
            .iter()
            .map(|(number, _, _)| {
                let block = history.next_block(*number);
                history.chain.push(block.block.hash);
                block
            })
            .collect();
        let results = Mutex::new(Vec::new());
        history.store.with_batch_session(|session| {
            for ((number, state, targets), block) in states.iter().zip(&blocks) {
                let result = Self::batch_block(session, *number, state, targets, *block);
                let failed = result.is_err();
                results.lock().expect("not poisoned").push(result);
                if failed {
                    break;
                }
            }
            Ok(())
        })?;
        let results = results.into_inner().expect("not poisoned");
        Ok(results
            .into_iter()
            .map(|result| {
                result.unwrap_or_else(|err| TrieBlockResult {
                    error: Some(format!("batch session: {err:#}")),
                    ..Default::default()
                })
            })
            .collect())
    }

    fn batch_block(
        session: &mut S::BatchSession<'_>,
        number: u64,
        state: &HashedPostState,
        targets: &[ProofTarget],
        block: BlockWithParent,
    ) -> eyre::Result<TrieBlockResult> {
        let provider = BaseProofsBatchStateProviderRef::new(
            Box::<NoopProvider>::default(),
            session,
            number - 1,
        );
        let (root, updates) = provider.state_root_with_updates(state.clone())?;
        // The session's other root paths at the parent: the stateless root from its execution
        // witness, which must agree.
        let mut mismatches = Vec::new();
        let parent_root = provider.state_root(HashedPostState::default())?;
        let nodes =
            provider.witness(TrieInput::default(), state.clone(), ExecutionWitnessMode::Legacy)?;
        let witness_root =
            WitnessState::new(parent_root, &nodes, &[], [], []).compute_state_root(state)?;
        if witness_root != root {
            mismatches.push(format!("witness root {witness_root}"));
        }
        drop(provider);
        let diff = BlockStateDiff {
            sorted_trie_updates: updates.into_sorted(),
            sorted_post_state: state.clone().into_sorted(),
        };
        session.store_trie_updates(block, diff)?;
        let provider =
            BaseProofsBatchStateProviderRef::new(Box::<NoopProvider>::default(), session, number);
        let mut request = MultiProofTargets::default();
        for target in targets {
            request.entry(target.key).or_default().extend(target.slots.iter().copied());
        }
        let multiproof = provider.multiproof(TrieInput::default(), request)?;
        let proofs = targets
            .iter()
            .map(|target| RethDb::proof(&multiproof, target))
            .collect::<eyre::Result<Vec<_>>>()?;
        // With preimages, the session's storage proof API must agree with its multiproof.
        if targets
            .iter()
            .all(|t| t.address.is_some() && (t.slots.is_empty() || t.slot_keys.is_some()))
        {
            let api = RethDb::storage_api_proofs(&provider, targets)?;
            mismatches.extend(api.error);
            if api.proofs.as_ref() != Some(&proofs) {
                mismatches.push("storage API proofs differ from the multiproof's".to_owned());
            }
        }
        Ok(TrieBlockResult {
            number: Some(number),
            state_root: Some(root),
            proofs: Some(proofs),
            error: (!mismatches.is_empty()).then(|| mismatches.join("; ")),
            ..Default::default()
        })
    }
}

/// Canonical block hashes by number, as the pruner's block hash reader.
#[derive(Debug)]
pub struct ChainHashes(pub Vec<B256>);

impl BlockHashReader for ChainHashes {
    fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        Ok(self.0.get(number as usize).copied())
    }

    fn canonical_hashes_range(&self, start: u64, end: u64) -> ProviderResult<Vec<B256>> {
        Ok((start..end).filter_map(|number| self.0.get(number as usize).copied()).collect())
    }
}
