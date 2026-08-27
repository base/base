//! State-provider instrumentation for block replay profiling.

use std::{collections::BTreeMap, time::Instant};

use alloy_primitives::{Address, B256, BlockNumber, Bytes, StorageKey, StorageValue};
use parking_lot::Mutex;
use reth_primitives_traits::{Account, Bytecode};
use reth_provider::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, ProviderResult,
    StateProofProvider, StateProvider, StateRootProvider, StorageRootProvider,
};
use reth_revm::db::BundleState;
use reth_trie::{
    AccountProof, ExecutionWitnessMode, HashedPostState, HashedStorage, MultiProof,
    MultiProofTargets, StorageMultiProof, StorageProof, TrieInput, updates::TrieUpdates,
};

use crate::{
    MeterStateProviderAccountAccess, MeterStateProviderCodeAccess, MeterStateProviderStats,
};

/// A state-provider wrapper that records parent-state fetch latency and accessed keys.
#[derive(Debug)]
pub struct MeteredStateProvider<S> {
    state_provider: S,
    stats: Mutex<MeterStateProviderStats>,
    accounts: Mutex<BTreeMap<Address, MeterStateProviderAccountAccess>>,
    code_hashes: Mutex<BTreeMap<B256, MeterStateProviderCodeAccess>>,
}

impl<S> MeteredStateProvider<S>
where
    S: StateProvider,
{
    /// Creates a metered wrapper around `state_provider`.
    pub fn new(state_provider: S) -> Self {
        Self {
            state_provider,
            stats: Mutex::new(MeterStateProviderStats::default()),
            accounts: Mutex::new(BTreeMap::new()),
            code_hashes: Mutex::new(BTreeMap::new()),
        }
    }

    /// Returns the cumulative fetch statistics.
    pub fn stats(&self) -> MeterStateProviderStats {
        *self.stats.lock()
    }

    /// Drains address and code-hash accesses recorded since the previous call.
    pub fn take_accesses(
        &self,
    ) -> (Vec<MeterStateProviderAccountAccess>, Vec<MeterStateProviderCodeAccess>) {
        let accounts = std::mem::take(&mut *self.accounts.lock()).into_values().collect();
        let code_hashes = std::mem::take(&mut *self.code_hashes.lock()).into_values().collect();
        (accounts, code_hashes)
    }
}

impl<S: AccountReader> AccountReader for MeteredStateProvider<S> {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        let start = Instant::now();
        let result = self.state_provider.basic_account(address);
        let elapsed_us = start.elapsed().as_micros();
        let bytecode_hash = result
            .as_ref()
            .ok()
            .and_then(|account| account.as_ref())
            .and_then(|account| account.bytecode_hash);
        let mut stats = self.stats.lock();
        stats.account_fetches = stats.account_fetches.saturating_add(1);
        stats.account_fetch_time_us = stats.account_fetch_time_us.saturating_add(elapsed_us);
        drop(stats);
        let mut accounts = self.accounts.lock();
        let access = accounts.entry(*address).or_insert_with(|| MeterStateProviderAccountAccess {
            address: *address,
            ..Default::default()
        });
        access.account_fetches = access.account_fetches.saturating_add(1);
        access.account_fetch_time_us = access.account_fetch_time_us.saturating_add(elapsed_us);
        access.bytecode_hash = bytecode_hash.or(access.bytecode_hash);
        result
    }
}

impl<S: StateProvider> StateProvider for MeteredStateProvider<S> {
    fn storage(
        &self,
        account: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        let start = Instant::now();
        let result = self.state_provider.storage(account, storage_key);
        let elapsed_us = start.elapsed().as_micros();
        let mut stats = self.stats.lock();
        stats.storage_fetches = stats.storage_fetches.saturating_add(1);
        stats.storage_fetch_time_us = stats.storage_fetch_time_us.saturating_add(elapsed_us);
        drop(stats);
        let mut accounts = self.accounts.lock();
        let access = accounts.entry(account).or_insert_with(|| MeterStateProviderAccountAccess {
            address: account,
            ..Default::default()
        });
        access.storage_fetches = access.storage_fetches.saturating_add(1);
        access.storage_fetch_time_us = access.storage_fetch_time_us.saturating_add(elapsed_us);
        access.storage_keys.insert(storage_key);
        result
    }
}

impl<S: BytecodeReader> BytecodeReader for MeteredStateProvider<S> {
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        let start = Instant::now();
        let result = self.state_provider.bytecode_by_hash(code_hash);
        let elapsed_us = start.elapsed().as_micros();
        let fetched_bytes = result
            .as_ref()
            .ok()
            .and_then(|code| code.as_ref().map(|code| code.0.len()))
            .unwrap_or_default() as u64;
        let mut stats = self.stats.lock();
        stats.code_fetches = stats.code_fetches.saturating_add(1);
        stats.code_fetch_time_us = stats.code_fetch_time_us.saturating_add(elapsed_us);
        stats.code_fetched_bytes = stats.code_fetched_bytes.saturating_add(fetched_bytes);
        drop(stats);
        let mut code_hashes = self.code_hashes.lock();
        let access = code_hashes.entry(*code_hash).or_insert_with(|| {
            MeterStateProviderCodeAccess { code_hash: *code_hash, ..Default::default() }
        });
        access.fetches = access.fetches.saturating_add(1);
        access.fetch_time_us = access.fetch_time_us.saturating_add(elapsed_us);
        access.fetched_bytes = access.fetched_bytes.saturating_add(fetched_bytes);
        result
    }
}

impl<S: StateRootProvider> StateRootProvider for MeteredStateProvider<S> {
    fn state_root(&self, hashed_state: HashedPostState) -> ProviderResult<B256> {
        self.state_provider.state_root(hashed_state)
    }

    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<B256> {
        self.state_provider.state_root_from_nodes(input)
    }

    fn state_root_with_updates(
        &self,
        hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        self.state_provider.state_root_with_updates(hashed_state)
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        self.state_provider.state_root_from_nodes_with_updates(input)
    }
}

impl<S: StateProofProvider> StateProofProvider for MeteredStateProvider<S> {
    fn proof(
        &self,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        self.state_provider.proof(input, address, slots)
    }

    fn multiproof(
        &self,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        self.state_provider.multiproof(input, targets)
    }

    fn witness(
        &self,
        input: TrieInput,
        target: HashedPostState,
        mode: ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        self.state_provider.witness(input, target, mode)
    }
}

impl<S: StorageRootProvider> StorageRootProvider for MeteredStateProvider<S> {
    fn storage_root(
        &self,
        address: Address,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        self.state_provider.storage_root(address, hashed_storage)
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        self.state_provider.storage_proof(address, slot, hashed_storage)
    }

    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        self.state_provider.storage_multiproof(address, slots, hashed_storage)
    }
}

impl<S: BlockHashReader> BlockHashReader for MeteredStateProvider<S> {
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        self.state_provider.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.state_provider.canonical_hashes_range(start, end)
    }
}

impl<S: HashedPostStateProvider> HashedPostStateProvider for MeteredStateProvider<S> {
    fn hashed_post_state(&self, bundle_state: &BundleState) -> ProviderResult<HashedPostState> {
        self.state_provider.hashed_post_state(bundle_state)
    }
}
