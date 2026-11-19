//! A reth node database (MDBX, with its static files and `RocksDB`) holding hashed state and trie
//! tables, and the root, proof, and witness code paths the node runs over it.

use std::{fs, ops::RangeInclusive, path::Path, sync::Arc};

use alloy_primitives::{B256, Bytes, U256};
use alloy_rlp::Decodable;
use alloy_trie::{Nibbles, TrieAccount, nodes::TrieNode};
use base_diff_adapter::WitnessState;
use base_diff_fuzz::{
    Preimages, ProofAccount, ProofResult, ProofTarget, StorageProofResult, TrieBlockResult,
};
use reth_chainspec::MAINNET;
use reth_codecs::Compact;
use reth_db::{
    DatabaseEnv,
    mdbx::{DatabaseArguments, MaxReadTransactionDuration},
};
use reth_db_api::{
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO},
    models::{AccountBeforeTx, BlockNumberAddress, StorageSettings},
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_primitives_traits::{Account, StorageEntry};
use reth_provider::{
    DatabaseProviderFactory, ProviderFactory, StateWriter, StorageSettingsCache, TrieWriter,
    providers::{RocksDBBuilder, StaticFileProviderBuilder},
    test_utils::MockNodeTypesWithDB,
};
use reth_stages_types::{MerkleCheckpoint, StorageRootMerkleCheckpoint};
use reth_storage_api::{StateProofProvider, StateRootProvider, StorageRootProvider};
use reth_trie::{
    IntermediateStateRootState, StateRoot, StateRootProgress, StorageRoot,
    trie_cursor::noop::NoopTrieCursorFactory,
    verify::{Output, Verifier},
};
use reth_trie_common::{
    AccountProof, EMPTY_ROOT_HASH, ExecutionWitnessMode, HashedPostState, HashedPostStateSorted,
    HashedStorage, MultiProof, MultiProofTargets, MultiProofTargetsV2, ProofV2Target, TrieInput,
    TrieInputSorted,
    updates::{TrieUpdates, TrieUpdatesSorted},
};
use reth_trie_db::{
    DatabaseHashedCursorFactory, DatabaseHashedPostState, DatabaseStateRoot, DatabaseStorageRoot,
    DatabaseTrieCursorFactory,
};

/// The provider factory of a [`RethDb`].
pub type RethFactory = ProviderFactory<MockNodeTypesWithDB<Arc<DatabaseEnv>>>;

/// A reth node database on disk.
#[derive(Debug)]
pub struct RethDb {
    /// The node's provider factory over the database.
    pub factory: RethFactory,
}

impl RethDb {
    /// Creates a database under `dir` with storage layout `settings` (v1 keys the trie tables
    /// by 65-byte nibble paths, v2 by packed 33-byte paths).
    pub fn open(
        dir: &Path,
        settings: StorageSettings,
        runtime: reth_tasks::Runtime,
    ) -> eyre::Result<Self> {
        let static_files = dir.join("static_files");
        fs::create_dir_all(&static_files)?;
        // Without the read transaction timeout, closing the environment leaves no monitor
        // thread behind, so an adapter that opens a database per request does not leak threads.
        let args = DatabaseArguments::default()
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded));
        let db = reth_db::init_db(dir.join("db"), args)?;
        let factory = ProviderFactory::new(
            Arc::new(db),
            MAINNET.clone(),
            StaticFileProviderBuilder::read_write(static_files).build()?,
            RocksDBBuilder::new(dir.join("rocksdb")).with_default_tables().build()?,
            runtime,
        )?;
        factory.set_storage_settings_cache(settings);
        Ok(Self { factory })
    }

    /// The storage slots of `key` in the database.
    pub fn slots(&self, key: &B256) -> eyre::Result<Vec<B256>> {
        let provider = self.factory.database_provider_ro()?;
        let mut cursor = provider.tx_ref().cursor_dup_read::<tables::HashedStorages>()?;
        let mut slots = Vec::new();
        for entry in cursor.walk_dup(Some(*key), None)? {
            slots.push(entry?.1.key);
        }
        Ok(slots)
    }

    /// The account at `key` in the database.
    pub fn account(&self, key: &B256) -> eyre::Result<Option<Account>> {
        let provider = self.factory.database_provider_ro()?;
        Ok(provider.tx_ref().get::<tables::HashedAccounts>(*key)?)
    }

    /// The node's serial state root with `state` on top of the database, as the payload builder
    /// and the engine's synchronous fallback compute it.
    pub fn serial_root(&self, state: &HashedPostState) -> eyre::Result<(B256, TrieUpdates)> {
        Ok(self.factory.latest()?.state_root_with_updates(state.clone())?)
    }

    /// The trie input of in-memory blocks given oldest first, merged newest to oldest as the
    /// storage overlay manager merges the executed blocks above its anchor.
    pub fn merge_memory(
        memory: &[(Arc<TrieUpdatesSorted>, Arc<HashedPostStateSorted>)],
    ) -> (TrieUpdatesSorted, HashedPostStateSorted) {
        let nodes = TrieUpdatesSorted::merge_batch(memory.iter().rev().map(|(n, _)| Arc::clone(n)));
        let state =
            HashedPostStateSorted::merge_batch(memory.iter().rev().map(|(_, s)| Arc::clone(s)));
        (Arc::unwrap_or_clone(nodes), Arc::unwrap_or_clone(state))
    }

    /// The serial state root with in-memory blocks (oldest first) between the database and
    /// `state`, as the overlay state provider computes it while blocks await persistence.
    pub fn overlay_root(
        &self,
        memory: &[(Arc<TrieUpdatesSorted>, Arc<HashedPostStateSorted>)],
        state: &HashedPostState,
    ) -> eyre::Result<(B256, TrieUpdates)> {
        let (nodes, mut overlay) = Self::merge_memory(memory);
        let current = state.clone().into_sorted();
        let prefix_sets = current.construct_prefix_sets();
        overlay.extend_ref_and_sort(&current);
        let input = TrieInputSorted::new(Arc::new(nodes), Arc::new(overlay), prefix_sets);
        let provider = self.factory.database_provider_ro()?;
        let (result, root_only) = reth_trie_db::with_adapter!(provider, |A| {
            type Root<'a, TX> = StateRoot<
                DatabaseTrieCursorFactory<&'a TX, A>,
                DatabaseHashedCursorFactory<&'a TX>,
            >;
            (
                <Root<'_, _> as DatabaseStateRoot<_>>::overlay_root_from_nodes_with_updates(
                    provider.tx_ref(),
                    input.clone(),
                ),
                <Root<'_, _> as DatabaseStateRoot<_>>::overlay_root_from_nodes(
                    provider.tx_ref(),
                    input,
                ),
            )
        });
        let (root, updates) = result?;
        let root_only = root_only?;
        eyre::ensure!(root_only == root, "overlay_root_from_nodes gave {root_only}, not {root}");
        Ok((root, updates))
    }

    /// Writes a block's hashed state and trie updates in one transaction, as persistence does.
    pub fn commit(&self, state: &HashedPostState, updates: TrieUpdates) -> eyre::Result<()> {
        let provider = self.factory.provider_rw()?;
        provider.write_hashed_state(&state.clone().into_sorted())?;
        provider.write_trie_updates(updates)?;
        provider.commit()?;
        Ok(())
    }

    /// Writes several blocks' hashed state and trie updates (oldest first) in one transaction,
    /// merged as engine persistence merges the blocks it saves when no state masking suffix
    /// stays in memory.
    pub fn commit_batch(
        &self,
        blocks: &[(Arc<TrieUpdatesSorted>, Arc<HashedPostStateSorted>)],
    ) -> eyre::Result<()> {
        let states: Vec<&HashedPostStateSorted> = blocks.iter().map(|(_, s)| &**s).collect();
        let nodes: Vec<&TrieUpdatesSorted> = blocks.iter().map(|(n, _)| &**n).collect();
        let provider = self.factory.provider_rw()?;
        provider
            .write_hashed_state(&HashedPostStateSorted::disjointed_merge_batch(&states, &[]))?;
        provider
            .write_trie_updates_sorted(&TrieUpdatesSorted::disjointed_merge_batch(&nodes, &[]))?;
        provider.commit()?;
        Ok(())
    }

    /// Writes a block's hashed state and the changesets execution leaves for the merkle stage:
    /// each changed account and each written slot as it was before the block, keyed by address
    /// and slot index. The trie tables stay as they were.
    pub fn write_with_changesets(
        &self,
        number: u64,
        state: &HashedPostState,
        preimages: &Preimages,
    ) -> eyre::Result<()> {
        let address = |key: &B256| {
            preimages.addresses.get(key).copied().ok_or_else(|| eyre::eyre!("no preimage of {key}"))
        };
        let provider = self.factory.provider_rw()?;
        let tx = provider.tx_ref();
        for (key, account) in &state.accounts {
            let info = tx.get::<tables::HashedAccounts>(*key)?;
            if info != *account {
                tx.put::<tables::AccountChangeSets>(
                    number,
                    AccountBeforeTx { address: address(key)?, info },
                )?;
            }
        }
        let mut cursor = tx.cursor_dup_read::<tables::HashedStorages>()?;
        for (key, storage) in &state.storages {
            let block_address = BlockNumberAddress((number, address(key)?));
            for slot in storage.storage.keys() {
                let value = cursor
                    .seek_by_key_subkey(*key, *slot)?
                    .filter(|entry| entry.key == *slot)
                    .map(|entry| entry.value)
                    .unwrap_or_default();
                let slot = preimages
                    .slots
                    .get(slot)
                    .copied()
                    .ok_or_else(|| eyre::eyre!("no preimage of slot {slot}"))?;
                tx.put::<tables::StorageChangeSets>(
                    block_address,
                    StorageEntry { key: slot, value },
                )?;
            }
        }
        provider.write_hashed_state(&state.clone().into_sorted())?;
        provider.commit()?;
        Ok(())
    }

    /// The merkle stage's incremental root over `range`: the trie tables advance from the
    /// range's parent to the hashed state, recomputing the keys the range's changesets name.
    pub fn merkle_incremental(&self, range: RangeInclusive<u64>) -> eyre::Result<B256> {
        let provider = self.factory.provider_rw()?;
        let (root, updates) = Self::incremental(&provider, range)?;
        provider.write_trie_updates(updates)?;
        provider.commit()?;
        Ok(root)
    }

    /// Unwinds the hashed state and trie tables from block `tip` to `target` as the hashing and
    /// merkle stages unwind: the hashed state takes the values the dropped blocks' changesets
    /// recorded, the incremental root recomputes the keys they name, and the changesets go.
    pub fn unwind_changesets(&self, target: u64, tip: u64) -> eyre::Result<B256> {
        let provider = self.factory.provider_rw()?;
        let reverts = HashedPostStateSorted::from_reverts(&*provider, target + 1..=tip)?;
        provider.write_hashed_state(&reverts)?;
        let (root, updates) = Self::incremental(&provider, target + 1..=tip)?;
        provider.write_trie_updates(updates)?;
        let tx = provider.tx_ref();
        for number in target + 1..=tip {
            tx.delete::<tables::AccountChangeSets>(number, None)?;
        }
        let mut cursor = tx.cursor_write::<tables::StorageChangeSets>()?;
        while let Some((key, _)) =
            cursor.seek(BlockNumberAddress((target + 1, Default::default())))?
        {
            if key.block_number() > tip {
                break;
            }
            cursor.delete_current()?;
        }
        provider.commit()?;
        Ok(root)
    }

    /// [`DatabaseStateRoot::incremental_root_with_updates`] over `provider`'s trie tables.
    fn incremental(
        provider: &reth_provider::DatabaseProviderRW<
            Arc<DatabaseEnv>,
            MockNodeTypesWithDB<Arc<DatabaseEnv>>,
        >,
        range: RangeInclusive<u64>,
    ) -> eyre::Result<(B256, TrieUpdates)> {
        Ok(reth_trie_db::with_adapter!(provider, |A| {
            <StateRoot<DatabaseTrieCursorFactory<_, A>, DatabaseHashedCursorFactory<_>> as DatabaseStateRoot<_>>::incremental_root_with_updates(&**provider, range)
        })?)
    }

    /// The state root the pipeline's merkle stage computes in rebuild mode, in a transaction that
    /// is rolled back: it clears the trie tables, then computes the root from the hashed tables in
    /// steps of at most `threshold` trie updates, writing each step's updates and resuming from its
    /// checkpoint after a round trip through the stage's compact checkpoint encoding.
    pub fn rebuild_root_with_progress(&self, threshold: u64) -> eyre::Result<B256> {
        let provider = self.factory.provider_rw()?;
        provider.tx_ref().clear::<tables::AccountsTrie>()?;
        provider.tx_ref().clear::<tables::StoragesTrie>()?;
        let mut checkpoint: Option<MerkleCheckpoint> = None;
        loop {
            let progress = reth_trie_db::with_adapter!(provider, |A| {
                StateRoot::<DatabaseTrieCursorFactory<_, A>, _>::from_tx(provider.tx_ref())
                    .with_threshold(threshold)
                    .with_intermediate_state(
                        checkpoint.take().map(IntermediateStateRootState::from),
                    )
                    .root_with_progress()
            })?;
            match progress {
                StateRootProgress::Progress(state, _, updates) => {
                    provider.write_trie_updates(updates)?;
                    let mut stored = MerkleCheckpoint::new(
                        0,
                        state.account_root_state.last_hashed_key,
                        state.account_root_state.walker_stack.into_iter().map(Into::into).collect(),
                        state.account_root_state.hash_builder.into(),
                    );
                    stored.storage_root_checkpoint = state.storage_root_state.map(|storage| {
                        StorageRootMerkleCheckpoint::new(
                            storage.state.last_hashed_key,
                            storage.state.walker_stack.into_iter().map(Into::into).collect(),
                            storage.state.hash_builder.into(),
                            storage.account,
                        )
                    });
                    let mut buf = Vec::new();
                    let len = stored.to_compact(&mut buf);
                    checkpoint = Some(MerkleCheckpoint::from_compact(&buf, len).0);
                }
                StateRootProgress::Complete(root, ..) => return Ok(root),
            }
        }
    }

    /// The state root recomputed from the hashed tables alone, ignoring stored trie nodes.
    pub fn full_root(&self) -> eyre::Result<B256> {
        let provider = self.factory.database_provider_ro()?;
        let root = StateRoot::new(
            NoopTrieCursorFactory::default(),
            DatabaseHashedCursorFactory::new(provider.tx_ref()),
        )
        .root()?;
        Ok(root)
    }

    /// The first stored trie node that differs from a rebuild from the hashed tables.
    pub fn verify(&self) -> eyre::Result<Option<String>> {
        let provider = self.factory.database_provider_ro()?;
        reth_trie_db::with_adapter!(provider, |A| {
            let trie = DatabaseTrieCursorFactory::<_, A>::new(provider.tx_ref());
            let hashed = DatabaseHashedCursorFactory::new(provider.tx_ref());
            for output in Verifier::new(&trie, hashed)? {
                match output? {
                    Output::Progress(_) => {}
                    inconsistency => return Ok(Some(format!("{inconsistency:?}"))),
                }
            }
            Ok(None)
        })
    }

    /// Storage roots of `keys` from the stored storage tries.
    pub fn storage_roots(&self, keys: &[B256]) -> eyre::Result<Vec<(B256, B256)>> {
        let provider = self.factory.database_provider_ro()?;
        keys.iter()
            .map(|key| {
                let root = reth_trie_db::with_adapter!(provider, |A| {
                    <StorageRoot<DatabaseTrieCursorFactory<_, A>, DatabaseHashedCursorFactory<_>> as DatabaseStorageRoot<_>>::from_tx_hashed(provider.tx_ref(), *key).root()
                })?;
                Ok((*key, root))
            })
            .collect()
    }

    /// Account and storage proofs of `targets` from one multiproof, as `eth_getProof` reads
    /// them.
    pub fn proofs(&self, targets: &[ProofTarget]) -> eyre::Result<Vec<ProofResult>> {
        let mut request = MultiProofTargets::default();
        for target in targets {
            request.entry(target.key).or_default().extend(target.slots.iter().copied());
        }
        let multiproof = self.factory.latest()?.multiproof(TrieInput::default(), request)?;
        targets.iter().map(|target| Self::proof(&multiproof, target)).collect()
    }

    /// The proofs of `targets`, which all carry preimages, as `eth_getProof` computes them: one
    /// account proof per address from `provider`.
    pub fn account_proofs(
        provider: &impl StateProofProvider,
        targets: &[ProofTarget],
    ) -> eyre::Result<Vec<ProofResult>> {
        targets
            .iter()
            .map(|target| {
                let address =
                    target.address.ok_or_else(|| eyre::eyre!("target without address"))?;
                let slot_keys = target.slot_keys.clone().unwrap_or_default();
                let proof = provider.proof(TrieInput::default(), address, &slot_keys)?;
                Ok(Self::account_proof_result(target, proof))
            })
            .collect()
    }

    /// The proofs of `targets`, which all carry preimages, as `eth_getMultiProof` computes them:
    /// one v2 multiproof over every hashed address and slot, then each address's account proof
    /// taken from it.
    pub fn multi_proofs(
        provider: &impl StateProofProvider,
        targets: &[ProofTarget],
    ) -> eyre::Result<Vec<ProofResult>> {
        let mut request = MultiProofTargetsV2::default();
        for target in targets {
            request.account_targets.push(ProofV2Target::new(target.key));
            if !target.slots.is_empty() {
                request
                    .storage_targets
                    .entry(target.key)
                    .or_default()
                    .extend(target.slots.iter().map(|slot| ProofV2Target::new(*slot)));
            }
        }
        let multiproof = provider.multiproof_v2(request, false)?;
        targets
            .iter()
            .map(|target| {
                let address =
                    target.address.ok_or_else(|| eyre::eyre!("target without address"))?;
                let slot_keys = target.slot_keys.clone().unwrap_or_default();
                let mut proof = multiproof.account_proof(address, &slot_keys)?;
                Self::trim_after_divergence(&mut proof.proof, &Nibbles::unpack(target.key))?;
                for (storage, slot) in proof.storage_proofs.iter_mut().zip(&target.slots) {
                    Self::trim_after_divergence(&mut storage.proof, &Nibbles::unpack(*slot))?;
                }
                Ok(Self::account_proof_result(target, proof))
            })
            .collect()
    }

    /// Drops the nodes after an extension whose key `path` leaves. The v2 multiproof's encoding
    /// (`encode_v2_proof_nodes`) emits the hashed branch below every extension on the way, so an
    /// absent key whose path leaves inside an extension gets that branch too, a node
    /// `eth_getProof` does not return. It is dropped here so the variant shows other differences.
    pub fn trim_after_divergence(proof: &mut Vec<Bytes>, path: &Nibbles) -> eyre::Result<()> {
        let mut depth = 0;
        for index in 0..proof.len() {
            match TrieNode::decode(&mut proof[index].as_ref())? {
                TrieNode::Branch(_) => depth += 1,
                TrieNode::Extension(extension) => {
                    if !path.slice(depth..).starts_with(&extension.key) {
                        proof.truncate(index + 1);
                        return Ok(());
                    }
                    depth += extension.key.len();
                }
                TrieNode::Leaf(_) | TrieNode::EmptyRoot => return Ok(()),
            }
        }
        Ok(())
    }

    /// `target`'s result from an account proof of its address and slot keys.
    pub fn account_proof_result(target: &ProofTarget, proof: AccountProof) -> ProofResult {
        ProofResult {
            key: target.key,
            account: proof.info.map(|info| ProofAccount {
                nonce: info.nonce,
                balance: info.balance,
                code_hash: info.get_bytecode_hash(),
                storage_root: proof.storage_root,
            }),
            account_proof: proof.proof,
            storage: target
                .slots
                .iter()
                .zip(proof.storage_proofs)
                .map(|(slot, storage)| StorageProofResult {
                    slot: *slot,
                    value: B256::from(storage.value.to_be_bytes::<32>()),
                    proof: storage.proof,
                })
                .collect(),
        }
    }

    /// [`Self::account_proofs`] with each slot's proof replaced by the storage API's
    /// (`storage_proof`), as `eth_getProof` for a storage-only request and the proofs-history RPC
    /// compute them. The result's error lists where `storage_multiproof` or `storage_root`
    /// disagree with them.
    pub fn storage_api_proofs(
        provider: &(impl StateProofProvider + StorageRootProvider),
        targets: &[ProofTarget],
    ) -> eyre::Result<TrieBlockResult> {
        let mut proofs = Self::account_proofs(provider, targets)?;
        let mut mismatches = Vec::new();
        for (target, result) in targets.iter().zip(&mut proofs) {
            let address = target.address.ok_or_else(|| eyre::eyre!("target without address"))?;
            let slot_keys = target.slot_keys.clone().unwrap_or_default();
            let multiproof =
                provider.storage_multiproof(address, &slot_keys, HashedStorage::default())?;
            for (key, storage) in slot_keys.iter().zip(&mut result.storage) {
                let proof = provider.storage_proof(address, *key, HashedStorage::default())?;
                if multiproof.storage_proof(*key)? != proof {
                    mismatches.push(format!("storage_multiproof of {address} slot {key}"));
                }
                storage.value = B256::from(proof.value.to_be_bytes::<32>());
                storage.proof = proof.proof;
            }
            let root = provider.storage_root(address, HashedStorage::default())?;
            let expected = result.account.as_ref().map_or(EMPTY_ROOT_HASH, |a| a.storage_root);
            if root != expected {
                mismatches.push(format!("storage_root of {address} is {root}, not {expected}"));
            }
            if multiproof.root != root {
                mismatches.push(format!("storage_multiproof root of {address}"));
            }
        }
        Ok(TrieBlockResult {
            proofs: Some(proofs),
            error: (!mismatches.is_empty()).then(|| mismatches.join("; ")),
            ..Default::default()
        })
    }

    /// The value of the leaf for `path` at the end of `proof`, if the proof shows it present.
    pub fn leaf_value(proof: &[Bytes], path: &Nibbles) -> eyre::Result<Option<Vec<u8>>> {
        let Some(last) = proof.last() else { return Ok(None) };
        if let TrieNode::Leaf(leaf) = TrieNode::decode(&mut last.as_ref())?
            && path.ends_with(&leaf.key)
        {
            return Ok(Some(leaf.value));
        }
        Ok(None)
    }

    /// One target's proofs out of a multiproof.
    pub fn proof(multiproof: &MultiProof, target: &ProofTarget) -> eyre::Result<ProofResult> {
        let path = Nibbles::unpack(target.key);
        let account_proof: Vec<Bytes> =
            multiproof.account_proof_nodes(&path).into_iter().map(|(_, node)| node).collect();
        let account = Self::leaf_value(&account_proof, &path)?
            .map(|value| TrieAccount::decode(&mut value.as_slice()))
            .transpose()?;
        let storage = target
            .slots
            .iter()
            .map(|slot| {
                let path = Nibbles::unpack(slot);
                let proof: Vec<Bytes> = multiproof
                    .storages
                    .get(&target.key)
                    .map(|storage| storage.subtree.matching_nodes_sorted(&path))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(_, node)| node)
                    .collect();
                let value = Self::leaf_value(&proof, &path)?
                    .map(|value| U256::decode(&mut value.as_slice()))
                    .transpose()?
                    .unwrap_or_default();
                Ok(StorageProofResult {
                    slot: *slot,
                    value: B256::from(value.to_be_bytes::<32>()),
                    proof,
                })
            })
            .collect::<eyre::Result<_>>()?;
        Ok(ProofResult {
            key: target.key,
            account: account.map(|a| ProofAccount {
                nonce: a.nonce,
                balance: a.balance,
                code_hash: a.code_hash,
                storage_root: a.storage_root,
            }),
            account_proof,
            storage,
        })
    }

    /// The state root of `state` on top of the database computed statelessly, as the
    /// differential execution adapter does: from the node's execution witness for `state` and
    /// the parent root alone.
    pub fn witness_root(
        &self,
        parent_root: B256,
        state: &HashedPostState,
        mode: ExecutionWitnessMode,
    ) -> eyre::Result<B256> {
        let nodes = self.factory.latest()?.witness(TrieInput::default(), state.clone(), mode)?;
        let witness = WitnessState::new(parent_root, &nodes, &[], [], []);
        Ok(witness.compute_state_root(state)?)
    }
}
